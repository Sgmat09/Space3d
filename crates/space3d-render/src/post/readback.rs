//! Non-blocking readback of post cameras' exposure (`FrameSettings::read_exposure`,
//! `Renderer::camera_exposure`).
//!
//! A frame that meters copies each metered camera's 1×1 exposure texture E into one slot of a
//! small ring of `MAP_READ` buffers (256 bytes per camera), mapped when the frame's submission
//! completes (`map_buffer_on_submit`); a later frame reads the mapped copy. The renderer never
//! waits for the GPU: the values are 1–3 frames old. Cameras with a manual exposure report it
//! directly.

use crate::handles::FastMap;
use space3d_core::Entity;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Copies in flight at most; when all are busy a frame is not read back.
const SLOTS: usize = 3;
/// Bytes per camera in a slot (texture-to-buffer copies at 256-byte offsets).
const STRIDE: u64 = 256;

const IDLE: u8 = 0;
const PENDING: u8 = 1;
const READY: u8 = 2;
const FAILED: u8 = 3;

#[derive(Default)]
struct Slot {
    buffer: Option<wgpu::Buffer>,
    /// Cameras the buffer holds room for.
    capacity: usize,
    state: Arc<AtomicU8>,
    /// Camera, exposure compensation (stops) and E format of each copy, in order.
    cameras: Vec<(Entity, f32, wgpu::TextureFormat)>,
}

/// Ring of exposure readback buffers (see the module docs).
#[derive(Default)]
pub(crate) struct ExposureReadback {
    slots: [Slot; SLOTS],
    /// The slot this frame's copies go to.
    active: Option<usize>,
    /// Cameras every slot holds room for.
    capacity: usize,
    /// Latest exposure (stops, compensation included) per camera, with the renderer frame it
    /// was stored in.
    values: FastMap<Entity, (f32, u64)>,
    tick: u64,
    /// Collections that grew (`RenderStats::cpu_reallocations`).
    pub reallocations: u32,
}

impl ExposureReadback {
    /// Read every copy the GPU has finished (never waits).
    pub fn collect(&mut self) {
        let tick = self.tick;
        for s in &mut self.slots {
            match s.state.load(Ordering::Acquire) {
                READY => {
                    let buffer = s.buffer.as_ref().expect("a mapped slot has a buffer");
                    if let Ok(view) = buffer.get_mapped_range(..) {
                        for (i, &(camera, compensation, format)) in s.cameras.iter().enumerate() {
                            let at = i * STRIDE as usize;
                            let ev = match format {
                                wgpu::TextureFormat::R16Float => {
                                    crate::instance::f16_to_f32(u16::from_le_bytes([view[at], view[at + 1]]))
                                }
                                _ => f32::from_le_bytes([view[at], view[at + 1], view[at + 2], view[at + 3]]),
                            };
                            let capacity = self.values.capacity();
                            self.values.insert(camera, (ev + compensation, tick));
                            self.reallocations += u32::from(self.values.capacity() != capacity);
                        }
                    }
                    buffer.unmap();
                    s.state.store(IDLE, Ordering::Release);
                }
                FAILED => s.state.store(IDLE, Ordering::Release),
                _ => {}
            }
        }
    }

    /// Start frame `tick` (renderer frames): collect finished copies.
    pub fn begin_frame(&mut self, tick: u64) {
        self.tick = tick;
        self.active = None;
        self.collect();
    }

    /// Reserve an idle slot for up to `count` copies this frame, mapped when `encoder`'s
    /// submission completes. Does nothing when every slot is in flight.
    pub fn begin(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, count: usize) {
        if count == 0 {
            return;
        }
        let Some(i) = self.slots.iter().position(|s| s.state.load(Ordering::Acquire) == IDLE) else {
            return;
        };
        // Every idle slot grows together: growth costs one frame, not one per slot.
        if self.slots[i].capacity < count {
            self.capacity = self.capacity.max(count.next_power_of_two());
        }
        for s in &mut self.slots {
            if s.capacity < self.capacity && s.state.load(Ordering::Acquire) == IDLE {
                s.capacity = self.capacity;
                s.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("space3d exposure readback"),
                    size: s.capacity as u64 * STRIDE,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                self.reallocations += 1;
            }
        }
        let s = &mut self.slots[i];
        s.cameras.clear();
        s.state.store(PENDING, Ordering::Release);
        let state = s.state.clone();
        let buffer = s.buffer.as_ref().expect("just ensured");
        encoder.map_buffer_on_submit(buffer, wgpu::MapMode::Read, .., move |r| {
            state.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release);
        });
        self.active = Some(i);
    }

    /// Copy `camera`'s exposure texture `texture` (of `format`) into this frame's slot;
    /// `compensation`: the stops added to it.
    pub fn record(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        camera: Entity,
        texture: &wgpu::Texture,
        format: wgpu::TextureFormat,
        compensation: f32,
    ) {
        let Some(s) = self.active.map(|i| &mut self.slots[i]) else {
            return;
        };
        let (Some(buffer), true) = (&s.buffer, s.cameras.len() < s.capacity) else {
            return;
        };
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: s.cameras.len() as u64 * STRIDE,
                    bytes_per_row: None,
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let capacity = s.cameras.capacity();
        s.cameras.push((camera, compensation, format));
        self.reallocations += u32::from(s.cameras.capacity() != capacity);
    }

    /// Store a camera's exposure known on the CPU (manual exposure).
    pub fn set(&mut self, camera: Entity, stops: f32) {
        let capacity = self.values.capacity();
        self.values.insert(camera, (stops, self.tick));
        self.reallocations += u32::from(self.values.capacity() != capacity);
    }

    /// The latest exposure of `camera` (stops, compensation included).
    pub fn get(&self, camera: Entity) -> Option<f32> {
        self.values.get(&camera).map(|v| v.0)
    }

    /// Forget values not refreshed for 120 renderer frames (cameras gone or no longer read).
    pub fn end_frame(&mut self) {
        let tick = self.tick;
        self.values.retain(|_, (_, t)| *t + 120 > tick);
    }
}
