//! The GPU simulation backend (design-particles §6, §7; graphics spec §5.2 PA2): the pool lives
//! in a storage buffer and the render node's `prepare` replays the tick log with compute
//! kernels in the frame's encoder, then compacts the visible particles into the render
//! instance stream with indirect draw arguments.
//!
//! Per frame (all recorded before the view passes, one encoder, uploads through the staging
//! ring):
//! * uploads: the entry table when entries changed, the spawn records and tick uniforms of the
//!   pending ticks, the compaction's jobs, groups and records when they changed;
//! * per pending tick: the emitter frames that changed (one copy), `simulate` and `emit` (2
//!   dispatches; `emit` only when something spawns; a `clear_buffer` between them for
//!   `OnRemove::Clear`);
//! * once, when anything changed: the block totals and draw arguments reset, `mark` and
//!   `scatter` (2 dispatches: the prefix sums live in `scatter`, see `compact.wgsl`), and a copy
//!   of the draw arguments for the alive-count readback.
//!
//! That is 4 dispatches for a frame with one tick, 2 more per extra tick (the budget is 5 + 2).
//! Kernels use `@workgroup_size(64)` and at most 4 storage buffers (GLES 3.1 / WebGPU minimums).
//!
//! Determinism: the kernels mirror `cpu::simulate`, `cpu::emit` and `cpu::pack` operation for
//! operation, spawn times and ages are integer-derived, and a particle's fate depends only on
//! `age >= life` of two exactly computed values, so the alive set and slots are identical to
//! the CPU backend's (`tests/parity.rs`); positions agree within float rounding. Output
//! positions come from sums of integers, never from the order of atomics.

mod readback;
pub(crate) mod tables;
#[cfg(test)]
mod tests;

use crate::cpu::{INSTANCE_SIZE, PackItem, PackLayer, Particle};
use crate::runtime::{Entry, Runtime};
use space3d_core::glam::{Quat, Vec3};
use space3d_render::{PrepareContext, wgpu};
use tables::{ENTRY_WORDS, GpuFrame, GpuSpawn, GpuTick, GpuXform, Word};

const COMMON_WGSL: &str = include_str!("../shaders/gpu_common.wgsl");
const SIMULATE_WGSL: &str = include_str!("../shaders/simulate.wgsl");
const COMPACT_WGSL: &str = include_str!("../shaders/compact.wgsl");

/// The per-tick kernels' source (`simulate`, `emit`).
pub(crate) fn simulate_shader() -> String {
    format!("{COMMON_WGSL}\n{SIMULATE_WGSL}")
}

/// The compaction kernels' source (`mark`, `scatter`).
pub(crate) fn compact_shader() -> String {
    format!("{COMMON_WGSL}\n{COMPACT_WGSL}")
}

/// Bytes per pool slot.
pub(crate) const PARTICLE_SIZE: u64 = std::mem::size_of::<Particle>() as u64;
/// Ring positions per compaction record (one workgroup).
const LANES: u32 = 64;
/// Largest workgroup count per grid dimension (WebGPU and GLES minimum).
const MAX_DIM: u32 = 65_535;

/// Largest pool the device can hold (slots): the storage binding and buffer size limits.
pub(crate) fn pool_limit(max_storage_binding: u64, max_buffer_size: u64) -> u32 {
    let bytes = max_storage_binding.min(max_buffer_size);
    let bytes = crate::testing::storage_limit().map_or(bytes, |l| l.min(bytes));
    (bytes / PARTICLE_SIZE).min(u32::MAX as u64) as u32
}

/// Workgroup grid for `n` workgroups within the per-dimension limit.
fn grid(n: u32) -> (u32, u32) {
    if n <= MAX_DIM {
        (n, 1)
    } else {
        (MAX_DIM, n.div_ceil(MAX_DIM))
    }
}

/// A grow-only buffer.
#[derive(Default)]
struct Buf {
    buffer: Option<wgpu::Buffer>,
    size: u64,
}

impl Buf {
    /// At least `size` bytes (a power of two ≥ 256); returns true when (re)created.
    fn ensure(&mut self, device: &wgpu::Device, size: u64, usage: wgpu::BufferUsages, label: &'static str) -> bool {
        if self.buffer.is_some() && self.size >= size {
            return false;
        }
        let size = size.max(256).next_power_of_two();
        self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.size = size;
        true
    }

    fn get(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("ensured before use")
    }
}

/// Counters of the last `prepare`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GpuFrameStats {
    pub ticks: u32,
    pub dispatches: u32,
    /// Compute passes recorded (`RenderStats::compute_passes`).
    pub passes: u32,
    pub bytes: u64,
    pub created: u32,
}

/// Inputs of [`GpuSim::prepare`] owned by the render node.
pub(crate) struct GpuInput<'a> {
    pub poses: &'a [(Vec3, Quat)],
    pub extrapolate: f32,
    pub dt: f32,
    pub max_catchup: u32,
    /// Upload this CPU pool first (the backend switched from CPU to GPU).
    pub carry: Option<&'a [Particle]>,
}

/// Timestamp queries of the perf tests: `(query set, first index)`; the first tick's sim pass
/// writes `first, first + 1`, the compaction pass `first + 2, first + 3`.
#[cfg(test)]
pub(crate) type TestTiming = (wgpu::QuerySet, u32);

/// Timestamp writes of one pass: query set, beginning and end indices.
type Stamp = Option<(wgpu::QuerySet, Option<u32>, Option<u32>)>;

fn writes(stamp: &Stamp) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
    stamp.as_ref().map(|(q, b, e)| wgpu::ComputePassTimestampWrites {
        query_set: q,
        beginning_of_pass_write_index: *b,
        end_of_pass_write_index: *e,
    })
}

/// GPU simulation state of one renderer.
pub(crate) struct GpuSim {
    pub renderer: u64,
    align: u64,
    sim_layout: wgpu::BindGroupLayout,
    compact_layout: wgpu::BindGroupLayout,
    simulate: wgpu::ComputePipeline,
    emit: wgpu::ComputePipeline,
    mark: wgpu::ComputePipeline,
    scatter: wgpu::ComputePipeline,
    pool: Buf,
    tables: Buf,
    xforms: Buf,
    spawns: Buf,
    ticks: Buf,
    frame: Buf,
    work: Buf,
    instances: Buf,
    sim_bind: Option<wgpu::BindGroup>,
    compact_bind: Option<wgpu::BindGroup>,
    /// Runtime entry version the entry section was built from.
    entries_version: Option<u64>,
    /// Entries the entry section has room for (jobs follow it).
    entry_cap: usize,
    /// Table words (entries, jobs, groups) and what the GPU buffer holds.
    words: Vec<Word>,
    uploaded: Vec<Word>,
    jobs_len: usize,
    records_at: usize,
    groups: u32,
    /// Blocks (64 records of a group) of the frame.
    blocks: u32,
    /// The reset block totals and draw arguments uploaded before each compaction.
    reset_words: Vec<u32>,
    records: u32,
    /// Emitter frames by instance, as on the GPU.
    xf: Vec<GpuXform>,
    spawn_list: Vec<GpuSpawn>,
    tick_bytes: Vec<u8>,
    /// Per entry: particles of the tick still to be written (scratch, `u32::MAX` untouched).
    keep: Vec<u32>,
    keys: Vec<(u32, u64, u32)>,
    group_words: Vec<Word>,
    frame_params: Option<GpuFrame>,
    pub readback: readback::AliveReadback,
    pub stats: GpuFrameStats,
    pub resets: u32,
    warned_catchup: bool,
    #[cfg(test)]
    pub test_timing: Option<TestTiming>,
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform(binding: u32, dynamic: bool, size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}

fn entry(binding: u32, buffer: &wgpu::Buffer, size: Option<u64>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer,
            offset: 0,
            size: size.and_then(wgpu::BufferSize::new),
        }),
    }
}

impl GpuSim {
    pub fn new(device: &wgpu::Device, renderer: u64) -> Self {
        let align = device.limits().min_uniform_buffer_offset_alignment.max(16) as u64;
        let tick_size = std::mem::size_of::<GpuTick>() as u64;
        let frame_size = std::mem::size_of::<GpuFrame>() as u64;
        let sim_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d particles simulate"),
            entries: &[
                storage(0, false),
                storage(1, true),
                storage(2, true),
                storage(3, true),
                uniform(4, true, tick_size),
            ],
        });
        let compact_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("space3d particles compact"),
            entries: &[
                storage(0, true),
                storage(1, true),
                storage(2, false),
                storage(3, false),
                uniform(4, false, frame_size),
            ],
        });
        let pipeline = |layout: &wgpu::BindGroupLayout, source: &str, entry: &'static str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(entry),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("space3d particles"),
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (sim_src, compact_src) = (simulate_shader(), compact_shader());
        Self {
            renderer,
            align,
            simulate: pipeline(&sim_layout, &sim_src, "simulate"),
            emit: pipeline(&sim_layout, &sim_src, "emit"),
            mark: pipeline(&compact_layout, &compact_src, "mark"),
            scatter: pipeline(&compact_layout, &compact_src, "scatter"),
            sim_layout,
            compact_layout,
            pool: Buf::default(),
            tables: Buf::default(),
            xforms: Buf::default(),
            spawns: Buf::default(),
            ticks: Buf::default(),
            frame: Buf::default(),
            work: Buf::default(),
            instances: Buf::default(),
            sim_bind: None,
            compact_bind: None,
            entries_version: None,
            entry_cap: 0,
            words: Vec::new(),
            uploaded: Vec::new(),
            jobs_len: 0,
            records_at: 0,
            groups: 0,
            blocks: 0,
            reset_words: Vec::new(),
            records: 0,
            xf: Vec::new(),
            spawn_list: Vec::new(),
            tick_bytes: Vec::new(),
            keep: Vec::new(),
            keys: Vec::new(),
            group_words: Vec::new(),
            frame_params: None,
            readback: readback::AliveReadback::default(),
            stats: GpuFrameStats::default(),
            resets: 0,
            warned_catchup: false,
            #[cfg(test)]
            test_timing: None,
        }
    }

    /// Scratch capacities (reallocation tracking).
    pub fn capacities(&self) -> usize {
        self.words.capacity()
            + self.uploaded.capacity()
            + self.xf.capacity()
            + self.spawn_list.capacity()
            + self.tick_bytes.capacity()
            + self.keep.capacity()
            + self.keys.capacity()
            + self.group_words.capacity()
            + self.reset_words.capacity()
    }

    /// The pool buffer (tests read it back).
    pub fn pool_buffer(&self) -> Option<&wgpu::Buffer> {
        self.pool.buffer.as_ref()
    }

    /// The work buffer (indirect args) and the instance buffer, once compacted.
    pub fn draw_buffers(&self) -> Option<(&wgpu::Buffer, &wgpu::Buffer)> {
        Some((self.work.buffer.as_ref()?, self.instances.buffer.as_ref()?))
    }

    /// Word offset of the block totals in the work buffer (after the records' masks and
    /// counts: 3 words per record).
    fn blocks_at(&self) -> u32 {
        3 * self.records
    }

    /// Byte offset of group `g`'s `DrawIndirectArgs` in the work buffer (after the block
    /// totals).
    pub fn args_offset(&self, g: u32) -> u64 {
        (self.blocks_at() as u64 + self.blocks as u64 + 4 * g as u64) * 4
    }

    /// Plan the draws: a job per drawable layer instance (in `(render layer, allocation
    /// serial)` order, like `cpu::pack`), a group per draw (all jobs of a batched layer, else one
    /// job), and the draw items with conservative bounds. Writes the jobs, then the groups, after
    /// the entry section of `words`.
    fn plan(
        &mut self,
        rt: &Runtime,
        layer_of: &dyn Fn(&Entry) -> Option<PackLayer>,
        input: &GpuInput<'_>,
        items: &mut Vec<PackItem>,
    ) {
        self.keys.clear();
        for (i, e) in rt.entries.iter().enumerate() {
            if !rt.may_be_alive(e, input.dt) || rt.instances[e.instance as usize].hidden {
                continue;
            }
            if let Some(l) = layer_of(e) {
                self.keys.push((l.id, e.serial, i as u32));
            }
        }
        self.keys.sort_unstable();
        let jobs_at = self.entry_cap * ENTRY_WORDS;
        self.words.truncate(jobs_at);
        self.group_words.clear();
        let (mut records, mut out, mut blocks) = (0u32, 0u32, 0u32);
        let mut k = 0;
        while k < self.keys.len() {
            let (id, serial, first) = self.keys[k];
            let e0 = &rt.entries[first as usize];
            let Some(layer) = layer_of(e0) else {
                k += 1;
                continue;
            };
            let end = if layer.batched {
                self.keys[k..]
                    .iter()
                    .position(|&(l, _, _)| l != id)
                    .map_or(self.keys.len(), |n| k + n)
            } else {
                k + 1
            };
            let g = self.group_words.len() as u32;
            let (first_out, first_record) = (out, records);
            let (mut min, mut max) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
            for &(_, _, ei) in &self.keys[k..end] {
                let e = &rt.entries[ei as usize];
                let inst = &rt.instances[e.instance as usize];
                let pose = input
                    .poses
                    .get(e.instance as usize)
                    .copied()
                    .unwrap_or((inst.pos, inst.rot));
                let cap = e.capacity.max(1);
                let tail = (e.spawned % cap as u64) as u32;
                let (tint, scale) = rt.draw_tint_scale(e);
                self.words
                    .extend(tables::job_words(ei, tail, records, g, tint, pose, scale));
                records += cap.div_ceil(LANES);
                out += cap;
                let (lo, hi) = rt.entry_bounds(e, input.poses, layer.extent);
                min = min.min(lo);
                max = max.max(hi);
            }
            self.group_words
                .push([first_record, records - first_record, first_out, blocks]);
            blocks += (records - first_record).div_ceil(LANES);
            items.push(PackItem {
                first: first_out,
                count: out - first_out,
                layer: id,
                serial,
                min,
                max,
                center: rt.entry_center(e0, input.poses),
                args: g,
            });
            k = end;
        }
        self.jobs_len = (self.words.len() - jobs_at) / 4;
        self.words.extend_from_slice(&self.group_words);
        self.groups = self.group_words.len() as u32;
        self.records = records;
        self.blocks = blocks;
        // Every record: its job, lanes in use and first slot, so `mark` and `scatter` find
        // their ring positions with one load.
        self.records_at = self.words.len();
        for j in 0..self.jobs_len {
            let [e, tail, _, _] = self.words[jobs_at + 4 * j];
            let entry = &rt.entries[e as usize];
            let cap = entry.capacity.max(1);
            for k in 0..cap.div_ceil(LANES) {
                let first = k * LANES;
                let lanes = (cap - first).min(LANES);
                let slot = entry.base + (tail + first) % cap;
                self.words.push([j as u32 | lanes << 24, slot, entry.base, cap]);
            }
        }
    }

    /// Timestamp writes for a pass: the frame's GPU timings (`PrepareContext::
    /// compute_timestamps`), or the perf tests' query set.
    fn stamp(&self, ctx: &mut PrepareContext<'_>, label: &'static str, test_index: u32) -> Stamp {
        let stamp = ctx.compute_timestamps(label).map(|w| {
            (
                w.query_set.clone(),
                w.beginning_of_pass_write_index,
                w.end_of_pass_write_index,
            )
        });
        #[cfg(test)]
        let stamp = stamp.or_else(|| {
            self.test_timing
                .as_ref()
                .map(|(q, i)| (q.clone(), Some(i + test_index), Some(i + test_index + 1)))
        });
        #[cfg(not(test))]
        let _ = test_index;
        stamp
    }

    /// Replay the pending ticks and compact; `items` receives the draws. `layer_of` maps a
    /// layer instance to its render layer (`None`: not drawn in color).
    pub fn prepare(
        &mut self,
        ctx: &mut PrepareContext<'_>,
        rt: &mut Runtime,
        layer_of: &dyn Fn(&Entry) -> Option<PackLayer>,
        input: &GpuInput<'_>,
        items: &mut Vec<PackItem>,
    ) {
        let mut stats = GpuFrameStats::default();
        self.readback.collect();

        // Entry section: rebuilt when entries changed (or outgrew their section).
        let n_entries = rt.entries.len();
        let jobs_at_before = self.entry_cap * ENTRY_WORDS;
        if n_entries > self.entry_cap {
            self.entry_cap = n_entries.max(64).next_power_of_two();
        }
        let entries_rebuilt =
            self.entry_cap * ENTRY_WORDS != jobs_at_before || self.entries_version != Some(rt.entries_version);
        if entries_rebuilt {
            self.entries_version = Some(rt.entries_version);
            self.words.resize(self.entry_cap * ENTRY_WORDS, [0; 4]);
            for (i, e) in rt.entries.iter().enumerate() {
                tables::entry_words(e, &mut self.words[i * ENTRY_WORDS..(i + 1) * ENTRY_WORDS]);
            }
        }
        items.clear();
        self.plan(rt, layer_of, input, items);

        let rebind = self.ensure_buffers(ctx, rt, items, &mut stats);
        let (rebind_sim, rebind_compact) = rebind;

        // Spawn records and tick uniforms of the pending ticks.
        let log = &rt.log;
        let max = input.max_catchup.max(1) as usize;
        let skip = log.frames.len().saturating_sub(max);
        if skip > 0 && !self.warned_catchup {
            self.warned_catchup = true;
            tracing::warn!(
                ticks = log.frames.len(),
                max_catchup = input.max_catchup,
                "particles: more pending ticks than ParticleSettings::max_catchup_ticks; the simulation restarts from the last ones (approximate)"
            );
        }
        let frames = &log.frames[skip..];
        let high = rt.pool.high_water();
        self.plan_ticks(rt, skip, high);
        let device = ctx.device;
        let stride = self.tick_stride();
        let mut rebind_sim = rebind_sim;
        if self.spawns.ensure(
            device,
            (self.spawn_list.len().max(1) * std::mem::size_of::<GpuSpawn>()) as u64,
            wgpu::BufferUsages::STORAGE,
            "space3d particles spawns",
        ) {
            stats.created += 1;
            rebind_sim = true;
        }
        if self.ticks.ensure(
            device,
            (self.tick_bytes.len() as u64).max(stride as u64),
            wgpu::BufferUsages::UNIFORM,
            "space3d particles ticks",
        ) {
            stats.created += 1;
            rebind_sim = true;
        }
        if rebind_sim {
            self.sim_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d particles simulate"),
                layout: &self.sim_layout,
                entries: &[
                    entry(0, self.pool.get(), None),
                    entry(1, self.tables.get(), None),
                    entry(2, self.xforms.get(), None),
                    entry(3, self.spawns.get(), None),
                    entry(4, self.ticks.get(), Some(std::mem::size_of::<GpuTick>() as u64)),
                ],
            }));
        }
        if rebind_compact {
            self.compact_bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("space3d particles compact"),
                layout: &self.compact_layout,
                entries: &[
                    entry(0, self.pool.get(), None),
                    entry(1, self.tables.get(), None),
                    entry(2, self.work.get(), None),
                    entry(3, self.instances.get(), None),
                    entry(4, self.frame.get(), None),
                ],
            }));
        }

        // Uploads: the changed table words (the entry section only when rebuilt), the carried
        // CPU pool, the tick data, the frame parameters.
        let jobs_at = self.entry_cap * ENTRY_WORDS;
        if self.uploaded.len() != self.words.len() {
            self.uploaded.resize(self.words.len(), [u32::MAX; 4]);
        }
        let mut tables_changed = false;
        let from = if entries_rebuilt || rebind_compact { 0 } else { jobs_at };
        let (new, old) = (&self.words[from..], &self.uploaded[from..]);
        if let Some(lo) = new.iter().zip(old).position(|(a, b)| a != b) {
            let hi = new.len() - new.iter().zip(old).rev().position(|(a, b)| a != b).unwrap_or(0);
            let (lo, hi) = (from + lo, from + hi);
            ctx.upload(
                self.tables.get(),
                lo as u64 * 16,
                bytemuck::cast_slice(&self.words[lo..hi]),
            );
            stats.bytes += (hi - lo) as u64 * 16;
            self.uploaded[lo..hi].copy_from_slice(&self.words[lo..hi]);
            tables_changed = true;
        }
        if let Some(carry) = input.carry {
            // The CPU pool continues on the GPU (whatever the GPU pool held before is stale).
            ctx.encoder.clear_buffer(self.pool.get(), 0, None);
            let n = carry.len().min((self.pool.size / PARTICLE_SIZE) as usize);
            if n > 0 {
                ctx.upload(self.pool.get(), 0, bytemuck::cast_slice(&carry[..n]));
                stats.bytes += n as u64 * PARTICLE_SIZE;
            }
        }
        if !frames.is_empty() {
            if !self.spawn_list.is_empty() {
                ctx.upload(self.spawns.get(), 0, bytemuck::cast_slice(&self.spawn_list));
                stats.bytes += (self.spawn_list.len() * std::mem::size_of::<GpuSpawn>()) as u64;
            }
            ctx.upload(self.ticks.get(), 0, &self.tick_bytes);
            stats.bytes += self.tick_bytes.len() as u64;
        }
        let params = GpuFrame {
            a: [
                jobs_at as u32,
                self.jobs_len as u32,
                (jobs_at + self.jobs_len * 4) as u32,
                self.groups,
            ],
            b: [
                self.records,
                (self.args_offset(0) / 4) as u32,
                2 * self.records,
                self.blocks_at(),
            ],
            c: [input.extrapolate, 0.0, 0.0, 0.0],
            d: [self.records_at as u32, 0, 0, 0],
        };
        let params_changed = self.frame_params != Some(params);
        if params_changed {
            ctx.upload(self.frame.get(), 0, bytemuck::bytes_of(&params));
            stats.bytes += std::mem::size_of::<GpuFrame>() as u64;
            self.frame_params = Some(params);
        }

        // Replay: per tick the emitter frames that changed, `simulate`, the tick's clears,
        // `emit`.
        let reset = !frames.is_empty() && (log.reset || skip > 0);
        if reset {
            ctx.encoder.clear_buffer(self.pool.get(), 0, None);
            self.resets += 1;
        }
        let pool_slots = (self.pool.size / PARTICLE_SIZE) as u32;
        let tick_size = std::mem::size_of::<GpuTick>();
        let xf_size = std::mem::size_of::<GpuXform>();
        for (t, f) in frames.iter().enumerate() {
            let (mut lo, mut hi) = (usize::MAX, 0);
            for row in &log.rows[f.rows.0 as usize..f.rows.1 as usize] {
                let i = row.instance as usize;
                let x = GpuXform::new(row);
                if let Some(slot) = self.xf.get_mut(i)
                    && *slot != x
                {
                    *slot = x;
                    lo = lo.min(i);
                    hi = hi.max(i + 1);
                }
            }
            if lo < hi {
                ctx.upload(
                    self.xforms.get(),
                    (lo * xf_size) as u64,
                    bytemuck::cast_slice(&self.xf[lo..hi]),
                );
                stats.bytes += ((hi - lo) * xf_size) as u64;
            }
            stats.ticks += 1;
            let tick: GpuTick = bytemuck::pod_read_unaligned(&self.tick_bytes[t * stride..t * stride + tick_size]);
            let simulate = f.dt > 0.0 && high > 0;
            let emit = tick.b[3] > 0;
            let clears = &log.clears[f.clears.0 as usize..f.clears.1 as usize];
            let offset = (t * stride) as u32;
            let split = !clears.is_empty();
            let first_pass = simulate || (emit && !split);
            if first_pass {
                let stamp = if t == 0 {
                    self.stamp(ctx, "particles.sim", 0)
                } else {
                    None
                };
                let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("particles.sim"),
                    timestamp_writes: writes(&stamp),
                });
                stats.passes += 1;
                pass.set_bind_group(0, self.sim_bind.as_ref(), &[offset]);
                if simulate {
                    pass.set_pipeline(&self.simulate);
                    let (x, y) = grid(high.div_ceil(LANES));
                    pass.dispatch_workgroups(x, y, 1);
                    stats.dispatches += 1;
                }
                if emit && !split {
                    pass.set_pipeline(&self.emit);
                    let (x, y) = grid(tick.b[3].div_ceil(LANES));
                    pass.dispatch_workgroups(x, y, 1);
                    stats.dispatches += 1;
                }
            }
            if split {
                for &(start, len) in clears {
                    let end = start.saturating_add(len).min(pool_slots);
                    let start = start.min(end);
                    if end > start {
                        ctx.encoder.clear_buffer(
                            self.pool.get(),
                            start as u64 * PARTICLE_SIZE,
                            Some((end - start) as u64 * PARTICLE_SIZE),
                        );
                    }
                }
                if emit {
                    let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("particles.emit"),
                        timestamp_writes: None,
                    });
                    stats.passes += 1;
                    pass.set_bind_group(0, self.sim_bind.as_ref(), &[offset]);
                    pass.set_pipeline(&self.emit);
                    let (x, y) = grid(tick.b[3].div_ceil(LANES));
                    pass.dispatch_workgroups(x, y, 1);
                    stats.dispatches += 1;
                }
            }
        }
        let ticked = !frames.is_empty();
        rt.log.clear();

        // Compaction, when anything it reads changed: reset the block totals and draw
        // arguments, then `mark` and `scatter`.
        if ticked || tables_changed || params_changed || input.carry.is_some() || rebind_compact {
            if self.records > 0 {
                self.reset_words.clear();
                self.reset_words.resize(self.blocks as usize, 0);
                for _ in 0..self.groups {
                    self.reset_words.extend_from_slice(&[4, 0, 0, 0]);
                }
                ctx.upload(
                    self.work.get(),
                    self.blocks_at() as u64 * 4,
                    bytemuck::cast_slice(&self.reset_words),
                );
                stats.bytes += self.reset_words.len() as u64 * 4;
                let (x, y) = grid(self.records);
                let stamp = self.stamp(ctx, "particles.compact", 2);
                let mut pass = ctx.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("particles.compact"),
                    timestamp_writes: writes(&stamp),
                });
                stats.passes += 1;
                pass.set_bind_group(0, self.compact_bind.as_ref(), &[]);
                pass.set_pipeline(&self.mark);
                pass.dispatch_workgroups(x, y, 1);
                pass.set_pipeline(&self.scatter);
                pass.dispatch_workgroups(x, y, 1);
                stats.dispatches += 2;
            }
            stats.created += self.readback.record(
                ctx.device,
                ctx.encoder,
                self.work.get(),
                self.args_offset(0),
                self.groups,
                ctx.frame.frame,
            );
        }
        ctx.count_dispatches(stats.dispatches);
        ctx.count_compute_passes(stats.passes);
        self.stats = stats;
    }

    /// Stride of the tick uniforms (the dynamic offset alignment).
    fn tick_stride(&self) -> usize {
        (std::mem::size_of::<GpuTick>() as u64).next_multiple_of(self.align) as usize
    }

    /// The spawn records and tick uniforms of the log's frames from `skip` on.
    fn plan_ticks(&mut self, rt: &Runtime, skip: usize, high: u32) {
        let log = &rt.log;
        let n_entries = rt.entries.len();
        let stride = self.tick_stride();
        self.spawn_list.clear();
        self.tick_bytes.clear();
        if self.keep.len() < n_entries {
            self.keep.resize(n_entries, u32::MAX);
        }
        for f in &log.frames[skip..] {
            let records = &log.records[f.records.0 as usize..f.records.1 as usize];
            let first = self.spawn_list.len();
            // Within a tick only the last `capacity` particles of a layer instance survive (its
            // ring wraps; the CPU writes and overwrites in order): drop the others, so no two
            // threads write the same slot.
            for r in records.iter().rev() {
                let Some(e) = rt.entries.get(r.entry as usize) else {
                    continue;
                };
                let keep = &mut self.keep[r.entry as usize];
                if *keep == u32::MAX {
                    *keep = e.capacity.max(1);
                }
                let kept = r.count.min(*keep);
                *keep -= kept;
                if kept == 0 {
                    continue;
                }
                let cap = e.capacity.max(1) as u64;
                self.spawn_list.push(GpuSpawn {
                    a: [r.entry, (r.first % cap) as u32, kept, 0],
                    b: [
                        r.first as u32,
                        (r.first >> 32) as u32,
                        crate::cpu::subtick_q(r.t0),
                        crate::cpu::subtick_q(r.t1),
                    ],
                    c: [r.count - kept, r.count, r.extra_age.to_bits(), 0],
                });
            }
            for r in records {
                if let Some(k) = self.keep.get_mut(r.entry as usize) {
                    *k = u32::MAX;
                }
            }
            // Back to log order, with each record's first thread.
            self.spawn_list[first..].reverse();
            let mut threads = 0;
            for s in &mut self.spawn_list[first..] {
                s.a[3] = threads;
                threads += s.a[2];
            }
            let attractors = &log.attractors[f.attractors.0 as usize..f.attractors.1 as usize];
            let mut tick = GpuTick::new(f.dt, f.substeps, f.time, attractors);
            tick.b[1] = first as u32;
            tick.b[2] = (self.spawn_list.len() - first) as u32;
            tick.b[3] = threads;
            tick.c[0] = high;
            tick.c[1] = n_entries as u32;
            self.tick_bytes.extend_from_slice(bytemuck::bytes_of(&tick));
            self.tick_bytes
                .resize(self.tick_bytes.len().next_multiple_of(stride), 0);
        }
    }

    /// Create or grow the buffers the frame needs (the pool keeps its particles); returns
    /// whether the simulate and compact bind groups must be rebuilt.
    fn ensure_buffers(
        &mut self,
        ctx: &mut PrepareContext<'_>,
        rt: &Runtime,
        items: &[PackItem],
        stats: &mut GpuFrameStats,
    ) -> (bool, bool) {
        let device = ctx.device;
        let storage = wgpu::BufferUsages::STORAGE;
        let (mut sim, mut compact) = (self.sim_bind.is_none(), self.compact_bind.is_none());
        let high = rt.pool.high_water().max(1) as u64;
        if self.pool.buffer.is_none() || self.pool.size < high * PARTICLE_SIZE {
            let limit = rt.pool.total().max(1) as u64 * PARTICLE_SIZE;
            let slots = (high * PARTICLE_SIZE)
                .max(self.pool.size + self.pool.size / 2)
                .min(limit.max(high * PARTICLE_SIZE))
                / PARTICLE_SIZE;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d particles pool"),
                size: slots.max(1) * PARTICLE_SIZE,
                usage: storage | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            if let Some(old) = &self.pool.buffer {
                // Growth keeps the particles: a GPU → GPU copy before this frame's replay.
                ctx.encoder.copy_buffer_to_buffer(old, 0, &buffer, 0, self.pool.size);
            }
            self.pool = Buf {
                size: buffer.size(),
                buffer: Some(buffer),
            };
            stats.created += 1;
            sim = true;
            compact = true;
        }
        let instances = rt.instances.len().max(1);
        if self.xf.len() < instances {
            self.xf.resize(instances, GpuXform::IDENTITY);
        }
        let xf_bytes = (self.xf.len() * std::mem::size_of::<GpuXform>()) as u64;
        if self
            .xforms
            .ensure(device, xf_bytes, storage, "space3d particles emitter frames")
        {
            stats.created += 1;
            sim = true;
            // A new buffer holds nothing: upload the whole mirror.
            ctx.upload(self.xforms.get(), 0, bytemuck::cast_slice(&self.xf));
            stats.bytes += xf_bytes;
        }
        if self.tables.ensure(
            device,
            (self.words.len() * 16) as u64,
            storage,
            "space3d particles tables",
        ) {
            stats.created += 1;
            sim = true;
            compact = true;
            self.uploaded.clear();
        }
        let work = self.args_offset(self.groups);
        let usage = storage | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_SRC;
        if self.work.ensure(device, work, usage, "space3d particles work") {
            stats.created += 1;
            compact = true;
        }
        let out: u64 = items.iter().map(|i| i.count as u64).sum();
        let usage = storage | wgpu::BufferUsages::VERTEX;
        if self
            .instances
            .ensure(device, out * INSTANCE_SIZE, usage, "space3d particles instances")
        {
            stats.created += 1;
            compact = true;
        }
        let size = std::mem::size_of::<GpuFrame>() as u64;
        if self
            .frame
            .ensure(device, size, wgpu::BufferUsages::UNIFORM, "space3d particles frame")
        {
            stats.created += 1;
            compact = true;
            self.frame_params = None;
        }
        (sim, compact)
    }
}
