//! `ParticleStats::alive` on the GPU backend: the groups' `DrawIndirectArgs` are copied into a
//! ring of three `MAP_READ` buffers, mapped when the frame's submission completes
//! (`map_buffer_on_submit`), and summed by a later frame. The frame never waits for the GPU; the
//! count is 1–3 frames old (`ParticleStats::alive_frame`).

use space3d_render::wgpu;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const SLOTS: usize = 3;
const IDLE: u8 = 0;
const PENDING: u8 = 1;
const READY: u8 = 2;
const FAILED: u8 = 3;
/// Bytes of one `DrawIndirectArgs`.
const ARGS: u64 = 16;

#[derive(Default)]
struct Slot {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    state: Arc<AtomicU8>,
    frame: u64,
    groups: u32,
}

/// The ring (see the module docs).
#[derive(Default)]
pub(crate) struct AliveReadback {
    slots: [Slot; SLOTS],
    /// Newest `(frame, alive)` read back.
    pub latest: Option<(u64, u32)>,
}

impl AliveReadback {
    /// Sum every copy the GPU has finished (never waits).
    pub fn collect(&mut self) {
        for s in &mut self.slots {
            match s.state.load(Ordering::Acquire) {
                READY => {
                    let buffer = s.buffer.as_ref().expect("a mapped slot has a buffer");
                    let bytes = s.groups as u64 * ARGS;
                    let alive = buffer.get_mapped_range(..bytes).ok().map(|view| {
                        view.as_chunks::<16>()
                            .0
                            .iter()
                            .map(|a| u32::from_le_bytes([a[4], a[5], a[6], a[7]]))
                            .sum::<u32>()
                    });
                    buffer.unmap();
                    s.state.store(IDLE, Ordering::Release);
                    if let Some(alive) = alive
                        && self.latest.is_none_or(|(f, _)| f < s.frame)
                    {
                        self.latest = Some((s.frame, alive));
                    }
                }
                FAILED => s.state.store(IDLE, Ordering::Release),
                _ => {}
            }
        }
    }

    /// Copy `groups` args from `args` (at `offset`) for frame `frame` into an idle slot; returns
    /// the buffers (re)allocated. Skipped when every slot is in flight.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        args: &wgpu::Buffer,
        offset: u64,
        groups: u32,
        frame: u64,
    ) -> u32 {
        if groups == 0 {
            self.latest = Some((frame, 0));
            return 0;
        }
        let Some(s) = self.slots.iter_mut().find(|s| s.state.load(Ordering::Acquire) == IDLE) else {
            return 0;
        };
        let bytes = groups as u64 * ARGS;
        let mut created = 0;
        if s.capacity < bytes {
            let size = bytes.max(s.capacity + s.capacity / 2).next_multiple_of(256);
            s.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("space3d particles alive readback"),
                size,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            s.capacity = size;
            created = 1;
        }
        let buffer = s.buffer.as_ref().expect("just ensured");
        encoder.copy_buffer_to_buffer(args, offset, buffer, 0, bytes);
        s.state.store(PENDING, Ordering::Release);
        s.frame = frame;
        s.groups = groups;
        let state = s.state.clone();
        encoder.map_buffer_on_submit(buffer, wgpu::MapMode::Read, ..bytes, move |r| {
            state.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release);
        });
        created
    }
}
