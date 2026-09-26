//! The GPU backend's per-view sort of `SortMode::ByDistance` draw groups (`shaders/sort.wgsl`,
//! design-particles §7 step 7): a bitonic sort of `(view depth, index)` keys per (camera view,
//! group), gathered into a region of a sorted instance buffer that the view draws with the
//! group's indirect arguments. Recorded in `prepare` after the compaction, in one compute pass
//! (`particles.sort`); skipped when neither the compacted instances nor the views changed.

use super::{Buf, entry, storage, uniform};
use crate::cpu::INSTANCE_SIZE;
use space3d_render::{PrepareContext, wgpu};

const SORT_WGSL: &str = include_str!("../shaders/sort.wgsl");

/// Largest group the GPU sorts (keys per job); larger groups draw unsorted (warned once).
pub(crate) const MAX_SORT: u32 = 65_536;
/// Keys a workgroup sorts in workgroup memory.
const BLOCK: u32 = 512;

/// `SortParams` of `sort.wgsl` (48 bytes, one aligned slot per dispatch).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct SortParams {
    a: [u32; 4],
    b: [u32; 4],
    row: [f32; 4],
}

/// One sort: a draw group of the GPU backend as one view sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SortJob {
    /// First instance of the group in the instance buffer (`PackItem::first`).
    pub src: u32,
    /// The group's capacity (`PackItem::count`; its instance count is at most this).
    pub capacity: u32,
    /// Word of the group's `DrawIndirectArgs::instance_count` in the work buffer.
    pub count_word: u32,
    /// The view's view-space z row (`dot(row.xyz, p) + row.w`).
    pub row: [f32; 4],
}

/// Keys of a job (its capacity rounded up to a power of two).
pub(crate) fn sort_size(capacity: u32) -> u32 {
    capacity.max(1).next_power_of_two()
}

/// Dispatches of a job of `n` keys: 1 up to a block, then per stage k ≥ 1024 the global steps
/// j = k/2 … 512 and one local merge.
#[cfg(test)]
pub(crate) fn sort_dispatches(n: u32) -> u32 {
    let mut d = 1;
    let mut k = 2 * BLOCK;
    while k <= n {
        d += (k / 2 / BLOCK).trailing_zeros() + 1 + 1;
        k *= 2;
    }
    d
}

/// A recorded dispatch: local or global, workgroups, params offset.
#[derive(Clone, Copy, Debug)]
struct Dispatch {
    global: bool,
    groups: u32,
    offset: u32,
}

/// The sort pipelines and buffers of one renderer.
pub(crate) struct GpuSort {
    align: u64,
    layout: wgpu::BindGroupLayout,
    local: wgpu::ComputePipeline,
    global: wgpu::ComputePipeline,
    keys: Buf,
    sorted: Buf,
    params: Buf,
    bind: Option<wgpu::BindGroup>,
    /// The instance and work buffers the bind group reads.
    bound: Option<(wgpu::Buffer, wgpu::Buffer)>,
    bytes: Vec<u8>,
    uploaded: Vec<u8>,
    dispatches: Vec<Dispatch>,
    /// The jobs of the last sort (skipped when unchanged and nothing was compacted).
    last: Vec<SortJob>,
    /// Dispatches recorded by the last `prepare`.
    pub recorded: u32,
}

impl GpuSort {
    pub fn new(device: &wgpu::Device) -> Self {
        let align = device.limits().min_uniform_buffer_offset_alignment.max(16) as u64;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d particles sort"),
            entries: &[
                storage(0, true),
                storage(1, true),
                storage(2, false),
                storage(3, false),
                uniform(4, true, std::mem::size_of::<SortParams>() as u64),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("space3d particles sort"),
            source: wgpu::ShaderSource::Wgsl(SORT_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("space3d particles sort"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &'static str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Self {
            align,
            local: pipeline("sort_local"),
            global: pipeline("sort_global"),
            layout,
            keys: Buf::default(),
            sorted: Buf::default(),
            params: Buf::default(),
            bind: None,
            bound: None,
            bytes: Vec::new(),
            uploaded: Vec::new(),
            dispatches: Vec::new(),
            last: Vec::new(),
            recorded: 0,
        }
    }

    /// Scratch capacities (reallocation tracking).
    pub fn capacities(&self) -> usize {
        self.bytes.capacity() + self.uploaded.capacity() + self.dispatches.capacity() + self.last.capacity()
    }

    /// The sorted instance buffer (jobs' regions in order: job `i` from the sum of the earlier
    /// jobs' capacities).
    pub fn sorted_buffer(&self) -> Option<&wgpu::Buffer> {
        self.sorted.buffer.as_ref()
    }

    /// Sort `jobs` (after the frame's compaction into `instances`, counts in `work`); `compacted`:
    /// the compaction ran this frame. Returns (buffers created, bytes uploaded).
    pub fn prepare(
        &mut self,
        ctx: &mut PrepareContext<'_>,
        (work, instances): (&wgpu::Buffer, &wgpu::Buffer),
        jobs: &[SortJob],
        compacted: bool,
    ) -> (u32, u64) {
        self.recorded = 0;
        if jobs.is_empty() {
            self.last.clear();
            return (0, 0);
        }
        let device = ctx.device;
        let (mut created, mut bytes) = (0u32, 0u64);
        let keys: u64 = jobs.iter().map(|j| sort_size(j.capacity) as u64).sum();
        let out: u64 = jobs.iter().map(|j| j.capacity as u64).sum();
        let storage = wgpu::BufferUsages::STORAGE;
        let mut rebind = self.bind.is_none() || self.bound.as_ref().is_none_or(|(i, w)| i != instances || w != work);
        if self
            .keys
            .ensure(device, keys * 8, storage, "space3d particles sort keys")
        {
            created += 1;
            rebind = true;
        }
        if self.sorted.ensure(
            device,
            out * INSTANCE_SIZE,
            storage | wgpu::BufferUsages::VERTEX,
            "space3d particles sorted instances",
        ) {
            created += 1;
            rebind = true;
            self.last.clear();
        }
        // Parameters: one aligned slot per dispatch.
        self.bytes.clear();
        self.dispatches.clear();
        let slot = (std::mem::size_of::<SortParams>() as u64).next_multiple_of(self.align) as usize;
        let (mut base, mut dst) = (0u32, 0u32);
        for job in jobs {
            let n = sort_size(job.capacity);
            let mut push = |bytes: &mut Vec<u8>, global: bool, groups: u32, k: u32, j: u32, last: bool| {
                let offset = bytes.len() as u32;
                let p = SortParams {
                    a: [base, n, job.count_word, job.src],
                    b: [dst, k, j, last as u32],
                    row: job.row,
                };
                bytes.extend_from_slice(bytemuck::bytes_of(&p));
                bytes.resize(offset as usize + slot, 0);
                self.dispatches.push(Dispatch { global, groups, offset });
            };
            let blocks = n.div_ceil(BLOCK);
            push(&mut self.bytes, false, blocks, 0, 0, n <= BLOCK);
            let mut k = 2 * BLOCK;
            while k <= n {
                let mut j = k / 2;
                while j >= BLOCK {
                    push(&mut self.bytes, true, (n / 2).div_ceil(64), k, j, false);
                    j /= 2;
                }
                push(&mut self.bytes, false, blocks, k, 0, k == n);
                k *= 2;
            }
            base += n;
            dst += job.capacity;
        }
        if self.params.ensure(
            device,
            self.bytes.len() as u64,
            wgpu::BufferUsages::UNIFORM,
            "space3d particles sort params",
        ) {
            created += 1;
            rebind = true;
            self.uploaded.clear();
        }
        if rebind {
            self.bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d particles sort"),
                layout: &self.layout,
                entries: &[
                    entry(0, instances, None),
                    entry(1, work, None),
                    entry(2, self.keys.get(), None),
                    entry(3, self.sorted.get(), None),
                    entry(4, self.params.get(), Some(std::mem::size_of::<SortParams>() as u64)),
                ],
            }));
            self.bound = Some((instances.clone(), work.clone()));
            created += 1;
            self.last.clear();
        }
        // Nothing to do when the instances and the views are what the sorted buffer was built
        // from.
        if !compacted && self.last.as_slice() == jobs {
            return (created, bytes);
        }
        self.last.clear();
        self.last.extend_from_slice(jobs);
        if self.uploaded != self.bytes {
            ctx.upload(self.params.get(), 0, &self.bytes);
            bytes += self.bytes.len() as u64;
            self.uploaded.clear();
            self.uploaded.extend_from_slice(&self.bytes);
        }
        let stamp = ctx.compute_timestamps("particles.sort");
        let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("particles.sort"),
            timestamp_writes: stamp,
        });
        for d in &self.dispatches {
            pass.set_pipeline(if d.global { &self.global } else { &self.local });
            pass.set_bind_group(0, self.bind.as_ref(), &[d.offset]);
            pass.dispatch_workgroups(d.groups, 1, 1);
        }
        drop(pass);
        self.recorded = self.dispatches.len() as u32;
        ctx.count_dispatches(self.recorded);
        ctx.count_compute_passes(1);
        (created, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_counts() {
        assert_eq!(sort_dispatches(1), 1);
        assert_eq!(sort_dispatches(512), 1);
        assert_eq!(sort_dispatches(1024), 3);
        assert_eq!(sort_dispatches(2048), 6);
        assert_eq!(sort_dispatches(16_384), 21);
        assert_eq!(sort_dispatches(65_536), 36);
        assert_eq!(sort_size(0), 1);
        assert_eq!(sort_size(300), 512);
    }

    /// The kernels validate and use at most 4 storage buffers.
    #[test]
    fn shader_validates() {
        let module =
            naga::front::wgsl::parse_str(SORT_WGSL).unwrap_or_else(|e| panic!("{}", e.emit_to_string(SORT_WGSL)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let storage = module
            .global_variables
            .iter()
            .filter(|(_, v)| matches!(v.space, naga::AddressSpace::Storage { .. }))
            .count();
        assert!(storage <= 4);
    }
}
