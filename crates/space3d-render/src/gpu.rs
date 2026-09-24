//! Persistent GPU buffers and the CPU→GPU upload path.
//!
//! * [`Uploader`] wraps a [`wgpu::util::StagingBelt`]: a ring of mapped staging chunks that are
//!   only reused once the GPU has consumed them (frame-in-flight safe, no write-after-read
//!   hazards, no per-frame staging allocations in the steady state). Data is written straight
//!   into mapped staging memory; large copies are split across rayon workers on native.
//! * [`GrowBuffer`] is a persistent GPU buffer with amortized growth (1.5x, 256-byte aligned).
//! * [`TrackedBuffer`] adds a CPU shadow copy; writes are diffed against it so only changed
//!   byte ranges are uploaded (e.g. unchanged camera → 0 bytes).

use crate::dirty::DirtyRanges;
use wgpu::util::StagingBelt;

/// Copies at or above this size are parallelized (native only).
#[cfg(not(target_arch = "wasm32"))]
const PARALLEL_COPY_THRESHOLD: usize = 1 << 20;

/// Round `v` up to a multiple of `align` (power of two).
pub(crate) fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

/// Per-frame upload context: staging belt + counters.
pub(crate) struct Uploader {
    belt: StagingBelt,
    /// Bytes written this frame.
    pub bytes: u64,
    /// Instance bytes written this frame.
    pub instance_bytes: u64,
    /// GPU buffers (re)allocated this frame.
    pub reallocations: u32,
}

impl Uploader {
    pub fn new(device: &wgpu::Device) -> Self {
        Self {
            // 4 MiB chunks: large enough for typical per-frame traffic, bigger writes get a
            // dedicated chunk that is recycled like any other.
            belt: StagingBelt::new(device.clone(), 4 << 20),
            bytes: 0,
            instance_bytes: 0,
            reallocations: 0,
        }
    }

    pub fn reset_counters(&mut self) {
        self.bytes = 0;
        self.instance_bytes = 0;
        self.reallocations = 0;
    }

    /// Copy `data` into `target` at `offset` through staging memory. `offset` and the length
    /// are padded to 4 bytes by the caller's layout (all our element sizes are multiples of 4).
    pub fn write(&mut self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::Buffer, offset: u64, data: &[u8]) {
        let Some(size) = wgpu::BufferSize::new(data.len() as u64) else {
            return;
        };
        debug_assert!(offset.is_multiple_of(4) && data.len().is_multiple_of(4), "unaligned upload");
        let mut view = self.belt.write_buffer(encoder, target, offset, size);
        copy_to_staging(view.slice(..), data);
        self.bytes += data.len() as u64;
    }

    /// Close staging chunks before submission.
    pub fn finish(&mut self) {
        self.belt.finish();
    }

    /// Recycle chunks after submission (they return once the GPU is done with them).
    pub fn recall(&mut self) {
        self.belt.recall();
    }
}

/// memcpy into write-combined staging memory, split across threads for large copies.
pub(crate) fn copy_to_staging(mut dst: wgpu::WriteOnly<'_, [u8]>, src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    #[cfg(not(target_arch = "wasm32"))]
    if src.len() >= PARALLEL_COPY_THRESHOLD {
        /// Raw pointer into the mapped range, handed to exactly one worker.
        struct SendPtr(std::ptr::NonNull<u8>);
        // SAFETY: each pointer addresses a disjoint sub-range of `dst`, which stays borrowed
        // (and therefore mapped and valid for writes) until `rayon::scope` returns.
        unsafe impl Send for SendPtr {}
        // SAFETY: only used to derive the per-task pointers above; never dereferenced shared.
        unsafe impl Sync for SendPtr {}

        let threads = rayon::current_num_threads().clamp(1, 8);
        let chunk = src.len().div_ceil(threads).next_multiple_of(4096);
        let base = SendPtr(dst.as_raw_element_ptr());
        let base = &base;
        rayon::scope(|s| {
            for (i, part) in src.chunks(chunk).enumerate() {
                // SAFETY: `i * chunk + part.len() <= dst.len()` because `src.len() == dst.len()`.
                let ptr = SendPtr(unsafe { base.0.add(i * chunk) });
                s.spawn(move |_| {
                    let ptr = ptr;
                    // SAFETY: see `SendPtr`; the range is exclusively owned by this task.
                    let mut w = unsafe {
                        wgpu::WriteOnly::<[u8]>::new(std::ptr::NonNull::slice_from_raw_parts(ptr.0, part.len()))
                    };
                    w.copy_from_slice(part);
                });
            }
        });
        return;
    }
    dst.copy_from_slice(src);
}

/// Persistent GPU buffer with amortized growth.
pub(crate) struct GrowBuffer {
    label: &'static str,
    usage: wgpu::BufferUsages,
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    /// Bumped on every reallocation so dependent bind groups can be rebuilt.
    pub generation: u32,
}

impl GrowBuffer {
    pub fn new(label: &'static str, usage: wgpu::BufferUsages) -> Self {
        Self {
            label,
            usage,
            buffer: None,
            capacity: 0,
            generation: 0,
        }
    }

    /// Ensure at least `size` bytes. Returns true if (re)allocated (contents lost).
    /// `exact` sizes the buffer tightly (static data), otherwise it grows 1.5x.
    pub fn ensure(&mut self, device: &wgpu::Device, size: u64, exact: bool, up: &mut Uploader) -> bool {
        let size = align_up(size.max(16), 16);
        if self.buffer.is_some() && size <= self.capacity {
            return false;
        }
        let capacity = if exact {
            align_up(size, 256)
        } else {
            align_up(size.max(self.capacity + self.capacity / 2), 256)
        };
        self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size: capacity,
            usage: self.usage,
            mapped_at_creation: false,
        }));
        self.capacity = capacity;
        self.generation = self.generation.wrapping_add(1);
        up.reallocations += 1;
        true
    }

    /// The buffer; panics if never allocated (callers `ensure` first).
    pub fn get(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("GrowBuffer used before ensure()")
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }
}

/// GPU buffer mirrored by a CPU shadow; writes are diffed and only changed ranges uploaded.
pub(crate) struct TrackedBuffer {
    pub gpu: GrowBuffer,
    shadow: Vec<u8>,
    len: usize,
    dirty: DirtyRanges,
    /// Set when the GPU copy must be fully rewritten (after reallocation).
    stale: bool,
}

impl TrackedBuffer {
    pub fn new(label: &'static str, usage: wgpu::BufferUsages) -> Self {
        Self {
            gpu: GrowBuffer::new(label, usage | wgpu::BufferUsages::COPY_DST),
            shadow: Vec::new(),
            len: 0,
            dirty: DirtyRanges::with_merge_gap(64),
            stale: true,
        }
    }

    /// Start rewriting from offset 0 (content beyond the new length is ignored).
    pub fn begin(&mut self) {
        self.len = 0;
    }

    /// Bytes in use.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Make room for `len` bytes, counting CPU reallocations.
    fn reserve_to(&mut self, len: usize, cpu_reallocs: &mut u32) {
        if len > self.shadow.len() {
            if len > self.shadow.capacity() {
                *cpu_reallocs += 1;
                self.shadow.reserve(len.max(self.shadow.capacity() * 3 / 2) - self.shadow.len());
            }
            // The GPU tail past the old shadow length was never written, so it is still
            // zero-initialized (wgpu guarantees this) and matches the zeroed shadow tail.
            self.shadow.resize(len, 0);
        }
    }

    /// Write `data` at `offset`, marking only bytes that differ from the shadow.
    pub fn write_at(&mut self, offset: usize, data: &[u8], cpu_reallocs: &mut u32) {
        let end = offset + data.len();
        self.reserve_to(end, cpu_reallocs);
        let dst = &mut self.shadow[offset..end];
        if dst != data {
            dst.copy_from_slice(data);
            self.dirty.mark(offset..end);
        }
        self.len = self.len.max(end);
    }

    /// Append at the current length; returns the offset written.
    pub fn push(&mut self, data: &[u8], cpu_reallocs: &mut u32) -> usize {
        let at = self.len;
        self.write_at(at, data, cpu_reallocs);
        at
    }

    /// Grow the used length to at least `n` bytes (zero-filled), so bindings of one full
    /// element stay valid even when nothing was written this frame.
    pub fn ensure_len(&mut self, n: usize, cpu_reallocs: &mut u32) {
        if self.len < n {
            self.reserve_to(n, cpu_reallocs);
            self.len = n;
        }
    }

    /// Upload changed ranges (or everything after reallocation). Returns bytes uploaded.
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        up: &mut Uploader,
    ) -> u64 {
        let len = self.len.next_multiple_of(4);
        if len > self.shadow.len() {
            self.shadow.resize(len, 0);
        }
        if self.gpu.ensure(device, len as u64, false, up) {
            self.stale = true;
        }
        let before = up.bytes;
        let buffer = self.gpu.get().clone();
        if self.stale {
            // Upload the whole shadow up to capacity-relevant length.
            let full = self.shadow.len().next_multiple_of(4).min(self.gpu.capacity() as usize);
            self.shadow.resize(full, 0);
            up.write(encoder, &buffer, 0, &self.shadow[..full]);
            self.stale = false;
        } else {
            for r in self.dirty.ranges() {
                let start = r.start & !3;
                let end = r.end.next_multiple_of(4).min(self.shadow.len());
                if start < end {
                    up.write(encoder, &buffer, start as u64, &self.shadow[start..end]);
                }
            }
        }
        self.dirty.clear();
        up.bytes - before
    }

    pub fn buffer(&self) -> &wgpu::Buffer {
        self.gpu.get()
    }
}
