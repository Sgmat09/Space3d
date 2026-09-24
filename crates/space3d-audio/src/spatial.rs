//! Spatial audio components and the backend-independent math/sync logic behind them.
use crate::volume::{PlayParams, SILENCE_DB, db_to_amplitude};
use kira::Easing;
use kira::sound::static_sound::StaticSoundData;
use space3d_core::GlobalTransform;
use space3d_core::glam::{Quat, Vec3};
use space3d_core::hecs::{Entity, World};
use std::f32::consts::FRAC_PI_8;

/// The "ears" of the scene. The active listener with the lowest entity id wins; its pose is
/// taken from `GlobalTransform` every `PostUpdate` (unrotated = facing -Z, +X right, +Y up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioListener {
    /// Inactive listeners are ignored.
    pub active: bool,
}

impl Default for AudioListener {
    fn default() -> Self {
        Self { active: true }
    }
}

/// World-space pose of the listener.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerPose {
    /// Position.
    pub position: Vec3,
    /// Orientation (unit quaternion).
    pub orientation: Quat,
}

impl ListenerPose {
    /// Extract position/orientation from a world matrix (scale is discarded).
    pub fn from_global(g: &GlobalTransform) -> Self {
        let (_, rotation, translation) = g.0.to_scale_rotation_translation();
        let orientation = if rotation.is_finite() && rotation.length_squared() > 0.0 {
            rotation.normalize()
        } else {
            Quat::IDENTITY
        };
        Self {
            position: translation,
            orientation,
        }
    }
}

impl Default for ListenerPose {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            orientation: Quat::IDENTITY,
        }
    }
}

/// How an emitter is heard. Distances and attenuation are fixed when the emitter's
/// spatial track is created; volume and strength can change any time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterSettings {
    /// Linear volume of the emitter track.
    pub volume: f32,
    /// Full volume at or below this distance.
    pub min_distance: f32,
    /// Silent at or beyond this distance.
    pub max_distance: f32,
    /// Distance falloff curve; `None` = constant volume.
    pub attenuation: Option<Easing>,
    /// 0 = no stereo panning, 1 = full ear-based panning.
    pub spatialization_strength: f32,
}

impl Default for EmitterSettings {
    fn default() -> Self {
        Self {
            volume: 1.0,
            min_distance: 1.0,
            max_distance: 100.0,
            attenuation: Some(Easing::Linear),
            spatialization_strength: 0.75,
        }
    }
}

/// Result of [`spatial_mix`]: what an emitter sounds like from the listener.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpatialMix {
    /// Listener–emitter distance.
    pub distance: f32,
    /// Distance attenuation as linear amplitude (0..=1).
    pub attenuation: f32,
    /// Left ear gain from direction (0..=1).
    pub left: f32,
    /// Right ear gain from direction (0..=1).
    pub right: f32,
}

impl SpatialMix {
    /// `(right - left)` in -1..=1: negative = heard on the left.
    pub fn balance(&self) -> f32 {
        self.right - self.left
    }
}

/// Evaluate a kira easing curve at `x` in 0..=1 (same formula kira uses internally).
pub fn ease(easing: Easing, x: f64) -> f64 {
    fn in_pow(x: f64, p: f64) -> f64 {
        x.powf(p)
    }
    let (kind, p) = match easing {
        Easing::Linear => return x,
        Easing::InPowi(p) => (0, p as f64),
        Easing::OutPowi(p) => (1, p as f64),
        Easing::InOutPowi(p) => (2, p as f64),
        Easing::InPowf(p) => (0, p),
        Easing::OutPowf(p) => (1, p),
        Easing::InOutPowf(p) => (2, p),
    };
    match kind {
        0 => in_pow(x, p),
        1 => 1.0 - in_pow(1.0 - x, p),
        _ => {
            let x = x * 2.0;
            if x < 1.0 {
                0.5 * in_pow(x, p)
            } else {
                0.5 * (1.0 - in_pow(2.0 - x, p)) + 0.5
            }
        }
    }
}

/// Compute how an emitter at `emitter` is heard from `listener`, mirroring kira's spatial
/// track model (distance attenuation in dB space + two directional ears).
pub fn spatial_mix(listener: &ListenerPose, emitter: Vec3, s: &EmitterSettings) -> SpatialMix {
    let distance = (listener.position - emitter).length();
    let attenuation = match s.attenuation {
        None => 1.0,
        Some(easing) => {
            let (min, max) = (s.min_distance, s.max_distance.max(s.min_distance + f32::EPSILON));
            let relative = (distance.clamp(min, max) - min) / (max - min);
            let relative_volume = ease(easing, (1.0 - relative) as f64) as f32;
            db_to_amplitude(SILENCE_DB + (0.0 - SILENCE_DB) * relative_volume)
        }
    };
    let strength = s.spatialization_strength.clamp(0.0, 1.0);
    let (left, right) = if strength == 0.0 {
        (1.0, 1.0)
    } else {
        const EAR_DISTANCE: f32 = 0.1;
        let o = listener.orientation;
        let left_pos = listener.position + o * (Vec3::NEG_X * EAR_DISTANCE);
        let right_pos = listener.position + o * (Vec3::X * EAR_DISTANCE);
        let left_dir = o * (Quat::from_rotation_y(-FRAC_PI_8) * Vec3::NEG_X);
        let right_dir = o * (Quat::from_rotation_y(FRAC_PI_8) * Vec3::X);
        let l = (left_dir.dot((emitter - left_pos).normalize_or_zero()) + 1.0) / 2.0;
        let r = (right_dir.dot((emitter - right_pos).normalize_or_zero()) + 1.0) / 2.0;
        let min_ear = 1.0 - strength;
        (min_ear + (1.0 - min_ear) * l, min_ear + (1.0 - min_ear) * r)
    };
    SpatialMix {
        distance,
        attenuation,
        left,
        right,
    }
}

/// A positional sound source. Queue sounds with [`AudioEmitter::play`]; they start on the
/// emitter's spatial track (under the `Sfx` bus) at the next `PostUpdate`, following the
/// entity's `GlobalTransform`. Removing the component or despawning the entity stops its
/// sounds.
#[derive(Clone, Default)]
pub struct AudioEmitter {
    /// Spatial settings.
    pub settings: EmitterSettings,
    pub(crate) queue: Vec<(StaticSoundData, PlayParams)>,
    pub(crate) position: Vec3,
    pub(crate) mix: Option<SpatialMix>,
}

impl std::fmt::Debug for AudioEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEmitter")
            .field("settings", &self.settings)
            .field("queued", &self.queue.len())
            .field("position", &self.position)
            .field("mix", &self.mix)
            .finish()
    }
}

impl AudioEmitter {
    /// Emitter with default settings (1..100 m linear falloff).
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all settings.
    pub fn with_settings(mut self, settings: EmitterSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Set the audible distance range.
    pub fn with_distances(mut self, min: f32, max: f32) -> Self {
        self.settings.min_distance = min;
        self.settings.max_distance = max;
        self
    }

    /// Queue a sound to start when the emitter is first synced (e.g. an ambient loop).
    pub fn with_sound(mut self, sound: StaticSoundData, params: PlayParams) -> Self {
        self.queue.push((sound, params));
        self
    }

    /// Play a one-shot at this emitter (starts next `PostUpdate`).
    pub fn play(&mut self, sound: StaticSoundData) {
        self.queue.push((sound, PlayParams::sfx()));
    }

    /// Play with explicit parameters (`bus` is ignored).
    pub fn play_with(&mut self, sound: StaticSoundData, params: PlayParams) {
        self.queue.push((sound, params));
    }

    /// Sounds waiting to start.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// World position at the last sync.
    pub fn position(&self) -> Vec3 {
        self.position
    }

    /// How this emitter was heard at the last sync (`None` without an active listener).
    pub fn mix(&self) -> Option<SpatialMix> {
        self.mix
    }
}

/// One emitter's state for a frame, handed to the backend.
pub(crate) struct EmitterFrame {
    pub entity: Entity,
    pub position: Vec3,
    pub settings: EmitterSettings,
    pub plays: Vec<(StaticSoundData, PlayParams)>,
}

/// Pose of the active listener (lowest entity id among active listeners).
pub fn find_listener(world: &World) -> Option<ListenerPose> {
    world
        .query::<(Entity, &AudioListener, &GlobalTransform)>()
        .iter()
        .filter(|(_, l, _)| l.active)
        .min_by_key(|(e, ..)| e.id())
        .map(|(_, _, g)| ListenerPose::from_global(g))
}

/// Update every emitter's position/mix from `GlobalTransform` and collect queued sounds.
pub(crate) fn collect_emitters(world: &mut World, listener: Option<ListenerPose>) -> Vec<EmitterFrame> {
    let mut out: Vec<EmitterFrame> = world
        .query_mut::<(Entity, &mut AudioEmitter, &GlobalTransform)>()
        .into_iter()
        .map(|(entity, em, g)| {
            let position = g.0.w_axis.truncate();
            em.position = position;
            em.mix = listener.map(|l| spatial_mix(&l, position, &em.settings));
            EmitterFrame {
                entity,
                position,
                settings: em.settings,
                plays: std::mem::take(&mut em.queue),
            }
        })
        .collect();
    out.sort_unstable_by_key(|f| f.entity.id());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener() -> ListenerPose {
        ListenerPose::default()
    }

    #[test]
    fn attenuation_follows_distance() {
        let s = EmitterSettings::default();
        let near = spatial_mix(&listener(), Vec3::new(0.0, 0.0, -0.5), &s);
        assert_eq!(near.attenuation, 1.0);
        let far = spatial_mix(&listener(), Vec3::new(0.0, 0.0, -150.0), &s);
        assert_eq!(far.attenuation, 0.0);
        // Halfway through the range with linear easing: -30 dB.
        let mid = spatial_mix(&listener(), Vec3::new(0.0, 0.0, -50.5), &s);
        assert!((mid.attenuation - db_to_amplitude(-30.0)).abs() < 1e-4, "{mid:?}");
        assert!((mid.distance - 50.5).abs() < 1e-4);
        let flat = EmitterSettings { attenuation: None, ..s };
        assert_eq!(spatial_mix(&listener(), Vec3::splat(1000.0), &flat).attenuation, 1.0);
    }

    #[test]
    fn panning_follows_listener_orientation() {
        let s = EmitterSettings::default();
        let right = spatial_mix(&listener(), Vec3::new(5.0, 0.0, 0.0), &s);
        assert!(right.right > right.left && right.balance() > 0.0, "{right:?}");
        let left = spatial_mix(&listener(), Vec3::new(-5.0, 0.0, 0.0), &s);
        assert!(left.balance() < 0.0);
        let ahead = spatial_mix(&listener(), Vec3::new(0.0, 0.0, -5.0), &s);
        assert!(ahead.balance().abs() < 1e-5);
        // Turn the listener 180° around Y: the right-hand source is now on the left.
        let turned = ListenerPose {
            orientation: Quat::from_rotation_y(std::f32::consts::PI),
            ..listener()
        };
        assert!(spatial_mix(&turned, Vec3::new(5.0, 0.0, 0.0), &s).balance() < 0.0);
        let mono = EmitterSettings {
            spatialization_strength: 0.0,
            ..s
        };
        let m = spatial_mix(&listener(), Vec3::new(5.0, 0.0, 0.0), &mono);
        assert_eq!((m.left, m.right), (1.0, 1.0));
    }

    #[test]
    fn easing_matches_endpoints() {
        for e in [
            Easing::Linear,
            Easing::InPowi(2),
            Easing::OutPowi(3),
            Easing::InOutPowi(2),
            Easing::InPowf(1.5),
            Easing::OutPowf(2.5),
            Easing::InOutPowf(3.0),
        ] {
            assert!(ease(e, 0.0).abs() < 1e-12, "{e:?}");
            assert!((ease(e, 1.0) - 1.0).abs() < 1e-12, "{e:?}");
        }
        assert_eq!(ease(Easing::InPowi(2), 0.5), 0.25);
        assert_eq!(ease(Easing::OutPowi(2), 0.5), 0.75);
        assert_eq!(ease(Easing::InOutPowi(2), 0.25), 0.125);
    }

    #[test]
    fn listener_pose_from_global() {
        let t = space3d_core::Transform::from_xyz(1.0, 2.0, 3.0)
            .with_rotation(Quat::from_rotation_y(1.0))
            .with_scale(Vec3::splat(3.0));
        let p = ListenerPose::from_global(&GlobalTransform(t.matrix()));
        assert!((p.position - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-5);
        assert!(p.orientation.angle_between(Quat::from_rotation_y(1.0)) < 1e-4);
    }
}
