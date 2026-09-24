//! Non-blocking readback of GPU culling results for [`crate::RenderStats`].
//!
//! On GPU culling paths the visible-instance counts only exist in the indirect args the cull
//! shader writes. Each frame that culls on the GPU copies its args (`instance_count` and
//! `index_count` of every culled batch, 20 bytes each) into a small ring of `MAP_READ`
//! buffers, mapped when the frame's submission completes (`map_buffer_on_submit`). A later
//! frame sums a mapped copy on the CPU and reports it: the renderer never waits for the GPU,
//! so the counts are those of an earlier frame — normally the previous one (1 frame of
//! latency), 2–3 frames when the GPU runs behind. [`crate::RenderStats::gpu_counts_frame`]
//! names the frame they come from.

use crate::gpu::align_up;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Copies in flight at most; when all are busy (GPU more than three frames behind) a frame is
/// simply not read back.
const SLOTS: usize = 3;
/// Size of one `DrawIndexedIndirectArgs`.
const ARGS_SIZE: u64 = 20;

/// Free for a new copy.
const IDLE: u8 = 0;
/// Copy recorded; mapped once the submission completes.
const PENDING: u8 = 1;
/// Mapped: ready to read.
const READY: u8 = 2;
/// Mapping failed (device lost); the slot is recycled without a result.
const FAILED: u8 = 3;

/// GPU-culled counts of one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CullCounts {
    /// Frame (`RenderStats::frame`) the counts were produced in.
    pub frame: u64,
    /// Instances submitted to GPU culling (sum over views).
    pub submitted: u64,
    /// Instances that survived GPU culling.
    pub visible: u64,
    /// Triangles of the visible instances.
    pub triangles: u64,
}

impl CullCounts {
    /// Sum a mapped copy of indirect args.
    pub fn from_args(frame: u64, submitted: u64, args: &[u8]) -> Self {
        let mut c = CullCounts {
            frame,
            submitted,
            ..Default::default()
        };
        for a in args.as_chunks::<{ ARGS_SIZE as usize }>().0 {
            let index_count = u32::from_le_bytes([a[0], a[1], a[2], a[3]]) as u64;
            let instances = u32::from_le_bytes([a[4], a[5], a[6], a[7]]) as u64;
            c.visible += instances;
            c.triangles += instances * (index_count / 3);
        }
        c
    }
}

#[derive(Default)]
struct Slot {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    state: Arc<AtomicU8>,
    frame: u64,
    draws: u32,
    submitted: u64,
}

/// Ring of readback buffers for the indirect args (see the module docs).
#[derive(Default)]
pub(crate) struct CullReadback {
    slots: [Slot; SLOTS],
    /// Size every slot buffer is (re)allocated with.
    capacity: u64,
    latest: Option<CullCounts>,
}

impl CullReadback {
    /// Read every copy the GPU has finished (never waits). The newest result is kept.
    pub fn collect(&mut self) {
        for s in &mut self.slots {
            match s.state.load(Ordering::Acquire) {
                READY => {
                    let buffer = s.buffer.as_ref().expect("a mapped slot has a buffer");
                    let bytes = s.draws as u64 * ARGS_SIZE;
                    let counts = match buffer.get_mapped_range(..bytes) {
                        Ok(view) => Some(CullCounts::from_args(s.frame, s.submitted, &view)),
                        Err(e) => {
                            tracing::warn!("culling stats readback failed: {e:?}");
                            None
                        }
                    };
                    buffer.unmap();
                    s.state.store(IDLE, Ordering::Release);
                    if let Some(c) = counts
                        && self.latest.is_none_or(|l| l.frame < c.frame)
                    {
                        self.latest = Some(c);
                    }
                }
                FAILED => s.state.store(IDLE, Ordering::Release),
                _ => {}
            }
        }
    }

    /// Newest counts read back so far.
    pub fn latest(&self) -> Option<CullCounts> {
        self.latest
    }

    /// Forget read-back counts (a frame without GPU culling: they no longer describe the scene).
    pub fn clear_latest(&mut self) {
        self.latest = None;
    }

    /// Copy the first `draws` indirect args of `args` for frame `frame` (whose GPU-culled
    /// batches held `submitted` instances) into an idle slot, mapped on submit. Does nothing
    /// when every slot is still in flight. Returns how many slot buffers were (re)allocated:
    /// all idle slots grow together (1.5×), so growth costs one frame, not one per slot.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        args: &wgpu::Buffer,
        draws: u32,
        submitted: u64,
        frame: u64,
    ) -> u32 {
        if draws == 0 {
            return 0;
        }
        let idle = |s: &Slot| s.state.load(Ordering::Acquire) == IDLE;
        let Some(i) = self.slots.iter().position(idle) else {
            return 0;
        };
        let bytes = draws as u64 * ARGS_SIZE;
        let mut reallocs = 0;
        if self.slots[i].capacity < bytes {
            self.capacity = align_up(bytes.max(self.capacity + self.capacity / 2), 256);
        }
        for s in self.slots.iter_mut().filter(|s| idle(s) && s.capacity < self.capacity) {
            s.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d culling stats readback"),
                size: self.capacity,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            s.capacity = self.capacity;
            reallocs += 1;
        }
        let s = &mut self.slots[i];
        let buffer = s.buffer.as_ref().expect("just ensured");
        encoder.copy_buffer_to_buffer(args, 0, buffer, 0, bytes);
        s.state.store(PENDING, Ordering::Release);
        s.frame = frame;
        s.draws = draws;
        s.submitted = submitted;
        let state = s.state.clone();
        encoder.map_buffer_on_submit(buffer, wgpu::MapMode::Read, ..bytes, move |r| {
            state.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release);
        });
        reallocs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_sum_instances_and_triangles() {
        let args: Vec<u8> = [[36u32, 5, 0, 0, 0], [6, 0, 36, 24, 0], [3, 7, 42, 28, 5]]
            .iter()
            .flat_map(|a| a.iter().flat_map(|w| w.to_le_bytes()))
            .collect();
        let c = CullCounts::from_args(9, 100, &args);
        assert_eq!(
            c,
            CullCounts {
                frame: 9,
                submitted: 100,
                visible: 12,
                triangles: 5 * 12 + 7,
            }
        );
    }
}
