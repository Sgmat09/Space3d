//! Cubemap registration in [`RenderAssets`] (handles, sources, face sizes). The GPU side —
//! upload, equirect conversion, the `TEXTURE_CUBE_MAP` path on GL — is the materials area's
//! (PM3); until then registered cubemaps stay [`EnvironmentState::Uploading`].

use super::api::{CubemapHandle, CubemapSource, EnvironmentState};
use crate::assets::RenderAssets;

/// A registered cubemap.
pub(crate) struct CubemapEntry {
    /// Pixel data waiting for upload.
    pub source: Option<CubemapSource>,
    /// Face edge in texels.
    pub face_size: u32,
    /// Bumped by every update.
    pub version: u32,
    /// Where the cubemap is on its way to being usable.
    pub state: EnvironmentState,
}

impl RenderAssets {
    /// Register a cubemap (uploaded, and converted from a panorama, by the renderer).
    pub fn add_cubemap(&mut self, source: CubemapSource) -> CubemapHandle {
        let h = CubemapHandle::from_raw(self.cubemaps.insert(CubemapEntry {
            face_size: source.face_size(),
            source: Some(source),
            version: 0,
            state: EnvironmentState::Uploading,
        }));
        self.pending_cubemaps.push(h);
        h
    }

    /// Replace a cubemap's contents (re-uploaded, environments re-baked). False for stale
    /// handles.
    pub fn update_cubemap(&mut self, h: CubemapHandle, source: CubemapSource) -> bool {
        let Some(e) = self.cubemaps.get_mut(h.raw()) else {
            return false;
        };
        e.face_size = source.face_size();
        e.source = Some(source);
        e.version = e.version.wrapping_add(1);
        e.state = EnvironmentState::Uploading;
        self.pending_cubemaps.push(h);
        true
    }

    /// Unregister a cubemap. False for stale handles.
    pub fn remove_cubemap(&mut self, h: CubemapHandle) -> bool {
        self.cubemaps.remove(h.raw()).is_some()
    }

    /// Whether `h` is a live cubemap.
    pub fn contains_cubemap(&self, h: CubemapHandle) -> bool {
        self.cubemaps.get(h.raw()).is_some()
    }

    /// Face edge of a cubemap in texels.
    pub fn cubemap_face_size(&self, h: CubemapHandle) -> Option<u32> {
        self.cubemaps.get(h.raw()).map(|e| e.face_size)
    }

    /// State of a cubemap (`Missing` for stale handles).
    #[allow(
        dead_code,
        reason = "graphics contract (spec §2.6): wired by the F4–F8 frame milestones"
    )]
    pub(crate) fn cubemap_state(&self, h: CubemapHandle) -> EnvironmentState {
        self.cubemaps
            .get(h.raw())
            .map_or(EnvironmentState::Missing, |e| e.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use space3d_core::ImageData;

    #[test]
    fn register_update_remove() {
        let mut a = RenderAssets::default();
        let faces = std::array::from_fn(|_| ImageData::new(8, 8));
        let h = a.add_cubemap(CubemapSource::from_faces(faces).unwrap());
        assert!(a.contains_cubemap(h));
        assert_eq!(a.cubemap_face_size(h), Some(8));
        assert_eq!(a.cubemap_state(h), EnvironmentState::Uploading);
        assert!(a.update_cubemap(h, CubemapSource::from_equirect_image(ImageData::new(64, 32))));
        assert_eq!(a.cubemap_face_size(h), Some(16));
        assert!(a.remove_cubemap(h) && !a.remove_cubemap(h));
        assert_eq!(a.cubemap_state(h), EnvironmentState::Missing);
        assert!(!a.update_cubemap(h, CubemapSource::from_equirect_image(ImageData::new(4, 2))));
    }
}
