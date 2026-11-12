//! GPU pass timings: [`GpuTimings`] (resource and [`crate::Renderer::gpu_timings`]), filled a
//! few frames late from timestamp queries shared by the frame, post, materials and nodes
//! (graphics spec §2.2.6, §4.4 labels; milestone F8).
//!
//! With `FrameSettings::gpu_timing` on a device offering `TIMESTAMP_QUERY`, every pass the
//! renderer records writes a timestamp at its start and end (pass-level `timestamp_writes`
//! only: no timestamps inside passes, which some backends lack). [`GpuTimer`] hands out the
//! query indices of one query set per timed frame. A frame's timestamps are resolved only once
//! its GPU work has completed (`Queue::on_submitted_work_done`), in a later frame's encoder:
//! Metal writes a render pass's boundary samples asynchronously, and a resolve recorded in the
//! same command buffer reads them before they land (measured: begin and end of the frame's
//! scene pass from different frames). The resolved copy is mapped when that later frame's
//! submission completes and read by the next frame: the timings are 2–5 frames old. Nothing
//! waits for the GPU and, after the first timed frames, the renderer allocates nothing for it
//! (the scope lists keep their capacity; the query sets and buffers of the ring are created
//! once); wgpu boxes the two callbacks per timed frame.

use space3d_core::Entity;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// GPU timestamps per pass, a few frames late (see the `FrameSettings::gpu_timing` docs).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct GpuTimings {
    /// The device offers `TIMESTAMP_QUERY` (and `FrameSettings::gpu_timing` can record).
    pub supported: bool,
    /// Frame (`RenderStats::frame`) the scopes were measured in.
    pub frame: Option<u64>,
    /// Sum of all scopes, in milliseconds (not the frame's GPU time: passes may overlap).
    pub total_ms: f32,
    /// One entry per timed pass, in recording order.
    pub scopes: Vec<GpuScope>,
}

impl GpuTimings {
    /// Milliseconds of every scope labelled `label` (0 without one).
    pub fn scope_ms(&self, label: &str) -> f32 {
        self.scopes.iter().filter(|s| s.label == label).map(|s| s.ms).sum()
    }
}

/// One timed pass.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct GpuScope {
    /// Pass label: `cull`, `shadow`, `scene` (every camera view's scene pass), `depth_resolve`,
    /// `scene_read`, `distortion`, `overlays.hdr`, `post.bloom`, `post.composite`, `post.fxaa`,
    /// `overlays`, and what the areas and nodes add (`ibl.bake`, `particles.sim`, …).
    pub label: &'static str,
    /// Camera the pass belongs to, if any.
    pub camera: Option<Entity>,
    /// GPU time in milliseconds.
    pub ms: f32,
}

/// Timestamps of one frame (one query set of this many; two per timed pass).
pub(crate) const MAX_TIMESTAMPS: u32 = 256;
/// Timed frames in the ring: the recording one, those whose GPU work or readback is pending.
/// With all of them busy a frame is simply not timed.
const FRAMES: usize = 5;

/// States of a ring entry.
const FREE: u8 = 0;
/// Recorded and submitted; waiting for its GPU work to complete.
const SUBMITTED: u8 = 1;
/// Its GPU work completed: resolve it in the next frame's encoder.
const DONE: u8 = 2;
/// Resolved and copied; mapped when that submission completes.
const RESOLVING: u8 = 3;
/// Mapped: ready to read.
const READY: u8 = 4;
/// Mapping failed (device lost): recycled without a result.
const FAILED: u8 = 5;
/// Being recorded.
const RECORDING: u8 = 6;

/// A timed pass (or span of passes): label, camera and its two query indices.
#[derive(Clone, Copy, Debug)]
struct Pending {
    label: &'static str,
    camera: Option<Entity>,
    begin: u32,
    end: u32,
}

/// One timed frame of the ring: its query set, resolve and readback buffers, state and scopes.
struct TimedFrame {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    state: Arc<AtomicU8>,
    frame: u64,
    count: u32,
    scopes: Vec<Pending>,
}

/// Hands out timestamp query indices while a frame is recorded and turns finished frames into
/// [`GpuTimings`] (see the module docs). Shared by reference with everything that records
/// passes (the frame, post, lighting and the render nodes' contexts): index allocation uses
/// interior mutability, so the timestamp writes it returns borrow only the timer.
pub(crate) struct GpuTimer {
    /// The device offers `TIMESTAMP_QUERY`.
    supported: bool,
    ring: Vec<TimedFrame>,
    /// Ring entry of the frame being recorded (`FrameSettings::gpu_timing`, until
    /// `end_frame`).
    current: Option<usize>,
    /// The frame being recorded is timed (mirror of `current` for `&self` access).
    recording: AtomicBool,
    next: AtomicU32,
    scopes: Mutex<Vec<Pending>>,
    frame: u64,
    /// Nanoseconds per timestamp tick.
    period: f32,
    /// Query sets and buffers created so far (3 per ring entry, created on the first timed
    /// frame; none after).
    pub objects_created: u32,
}

impl GpuTimer {
    pub fn new(supported: bool, period: f32) -> Self {
        Self {
            supported,
            ring: Vec::new(),
            current: None,
            recording: AtomicBool::new(false),
            next: AtomicU32::new(0),
            scopes: Mutex::new(Vec::new()),
            frame: 0,
            period,
            objects_created: 0,
        }
    }

    /// Start recording frame `frame` when `enabled` (and supported, and a ring entry is
    /// free): passes get timestamp writes until [`Self::end_frame`].
    pub fn begin_frame(&mut self, device: &wgpu::Device, enabled: bool, frame: u64) {
        self.current = None;
        self.next.store(0, Ordering::Relaxed);
        self.scopes.get_mut().unwrap_or_else(|e| e.into_inner()).clear();
        self.frame = frame;
        if enabled && self.supported && self.ring.is_empty() {
            let size = MAX_TIMESTAMPS as u64 * 8;
            let buffer = |label, usage| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size,
                    usage,
                    mapped_at_creation: false,
                })
            };
            use wgpu::BufferUsages as U;
            self.ring = (0..FRAMES)
                .map(|_| TimedFrame {
                    set: device.create_query_set(&wgpu::QuerySetDescriptor {
                        label: Some("space3d gpu timings"),
                        ty: wgpu::QueryType::Timestamp,
                        count: MAX_TIMESTAMPS,
                    }),
                    resolve: buffer("space3d gpu timings resolve", U::QUERY_RESOLVE | U::COPY_SRC),
                    readback: buffer("space3d gpu timings readback", U::MAP_READ | U::COPY_DST),
                    state: Arc::new(AtomicU8::new(FREE)),
                    frame: 0,
                    count: 0,
                    scopes: Vec::with_capacity(MAX_TIMESTAMPS as usize / 2),
                })
                .collect();
            self.scopes
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .reserve(MAX_TIMESTAMPS as usize / 2);
            self.objects_created += 3 * FRAMES as u32;
        }
        if enabled && self.supported {
            self.current = self.ring.iter().position(|t| t.state.load(Ordering::Acquire) == FREE);
            if let Some(i) = self.current {
                self.ring[i].state.store(RECORDING, Ordering::Release);
            }
        }
        self.recording.store(self.current.is_some(), Ordering::Relaxed);
    }

    /// Two query indices for a pass labelled `label`, recorded as a scope (`None` when the
    /// frame is not timed or its query set is full).
    fn allocate(&self, label: &'static str, camera: Option<Entity>) -> Option<(&wgpu::QuerySet, u32)> {
        if !self.recording.load(Ordering::Relaxed) {
            return None;
        }
        let set = &self.ring[self.current?].set;
        let begin = self.next.fetch_add(2, Ordering::Relaxed);
        if begin + 2 > MAX_TIMESTAMPS {
            self.next.store(MAX_TIMESTAMPS, Ordering::Relaxed);
            return None;
        }
        let mut scopes = self.scopes.lock().unwrap_or_else(|e| e.into_inner());
        scopes.push(Pending {
            label,
            camera,
            begin,
            end: begin + 1,
        });
        Some((set, begin))
    }

    /// Timestamp writes for a render pass labelled `label` (`None` when not timing).
    pub fn render(&self, label: &'static str, camera: Option<Entity>) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.allocate(label, camera)
            .map(|(query_set, i)| wgpu::RenderPassTimestampWrites {
                query_set,
                beginning_of_pass_write_index: Some(i),
                end_of_pass_write_index: Some(i + 1),
            })
    }

    /// Timestamp writes for a compute pass labelled `label` (`None` when not timing).
    pub fn compute(&self, label: &'static str, camera: Option<Entity>) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        self.allocate(label, camera)
            .map(|(query_set, i)| wgpu::ComputePassTimestampWrites {
                query_set,
                beginning_of_pass_write_index: Some(i),
                end_of_pass_write_index: Some(i + 1),
            })
    }

    /// One scope spanning several passes: the query set and the indices to write at the start
    /// of the first pass and at the end of the last one.
    pub fn span(&self, label: &'static str, camera: Option<Entity>) -> Option<(&wgpu::QuerySet, u32, u32)> {
        self.allocate(label, camera).map(|(q, i)| (q, i, i + 1))
    }

    /// End of the frame's recording (before its submission): resolve the earlier timed frames
    /// whose GPU work completed and copy them into their readback buffers, mapped when this
    /// submission completes. Passes recorded later (capture replays) are not timed.
    pub fn end_frame(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.recording.store(false, Ordering::Relaxed);
        for t in &mut self.ring {
            if t.state.load(Ordering::Acquire) != DONE {
                continue;
            }
            let bytes = t.count as u64 * 8;
            encoder.resolve_query_set(&t.set, 0..t.count, &t.resolve, 0);
            encoder.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, bytes);
            t.state.store(RESOLVING, Ordering::Release);
            let state = t.state.clone();
            encoder.map_buffer_on_submit(&t.readback, wgpu::MapMode::Read, ..bytes, move |r| {
                state.store(if r.is_ok() { READY } else { FAILED }, Ordering::Release);
            });
        }
        let Some(i) = self.current else { return };
        let count = self.next.load(Ordering::Relaxed).min(MAX_TIMESTAMPS);
        let scopes = self.scopes.get_mut().unwrap_or_else(|e| e.into_inner());
        let t = &mut self.ring[i];
        if count == 0 || scopes.is_empty() {
            t.state.store(FREE, Ordering::Release);
            self.current = None;
            return;
        }
        t.frame = self.frame;
        t.count = count;
        std::mem::swap(&mut t.scopes, scopes);
    }

    /// After the frame's submission: its timestamps are resolved once that work completes.
    pub fn submitted(&mut self, queue: &wgpu::Queue) {
        let Some(i) = self.current.take() else { return };
        let state = self.ring[i].state.clone();
        state.store(SUBMITTED, Ordering::Release);
        queue.on_submitted_work_done(move || state.store(DONE, Ordering::Release));
    }

    /// Turn the newest read-back frame into `timings` (never waits). Returns whether `timings`
    /// changed.
    pub fn collect(&mut self, timings: &mut GpuTimings) -> bool {
        let period = self.period as f64;
        let mut changed = false;
        for t in &mut self.ring {
            match t.state.load(Ordering::Acquire) {
                READY => {
                    let bytes = t.count as u64 * 8;
                    if timings.frame.is_none_or(|f| f < t.frame)
                        && let Ok(view) = t.readback.get_mapped_range(..bytes)
                    {
                        let ticks = |i: u32| {
                            let at = i as usize * 8;
                            u64::from_le_bytes(view[at..at + 8].try_into().expect("8 bytes"))
                        };
                        timings.scopes.clear();
                        timings.total_ms = 0.0;
                        for p in &t.scopes {
                            // A timestamp never written (a pass that was not recorded) reads 0,
                            // or Metal's error value `u64::MAX`: that scope reports 0.
                            let (b, e) = (ticks(p.begin), ticks(p.end));
                            let valid = b != 0 && e != u64::MAX && e >= b;
                            let ms = if valid {
                                ((e - b) as f64 * period / 1e6) as f32
                            } else {
                                0.0
                            };
                            timings.total_ms += ms;
                            timings.scopes.push(GpuScope {
                                label: p.label,
                                camera: p.camera,
                                ms,
                            });
                        }
                        timings.frame = Some(t.frame);
                        changed = true;
                    }
                    t.readback.unmap();
                    t.state.store(FREE, Ordering::Release);
                }
                FAILED => t.state.store(FREE, Ordering::Release),
                _ => {}
            }
        }
        changed
    }
}

/// A part of the frame's CPU work, timed while the returned guard lives: a trace span of the
/// kind the schedule opens round every system (`"system"`, with a `name`), so the profiler
/// lists it beside them ("render.prepare"). Costs nothing while no subscriber takes trace spans.
pub(crate) fn part(name: &'static str) -> tracing::span::EnteredSpan {
    tracing::trace_span!("system", name = name).entered()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ms_sums_by_label() {
        let t = GpuTimings {
            supported: true,
            frame: Some(3),
            total_ms: 0.6,
            scopes: vec![
                GpuScope {
                    label: "scene",
                    camera: None,
                    ms: 0.25,
                },
                GpuScope {
                    label: "post.bloom",
                    camera: None,
                    ms: 0.1,
                },
                GpuScope {
                    label: "scene",
                    camera: None,
                    ms: 0.25,
                },
            ],
        };
        assert_eq!(t.scope_ms("scene"), 0.5);
        assert_eq!(t.scope_ms("cull"), 0.0);
    }

    #[test]
    fn a_timer_off_hands_out_nothing() {
        let t = GpuTimer::new(false, 1.0);
        assert!(t.render("scene", None).is_none());
        assert!(t.compute("cull", None).is_none());
        assert!(t.span("post.bloom", None).is_none());
    }
}
