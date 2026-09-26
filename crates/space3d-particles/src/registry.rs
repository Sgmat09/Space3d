//! Effect registry: [`EffectHandle`]s, the [`ParticleEffects`] resource and the [`builtin`]
//! library.

use crate::effect::ParticleEffect;

/// Handle to an effect in [`ParticleEffects`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffectHandle {
    index: u32,
    generation: u32,
}

impl EffectHandle {
    /// A handle no effect ever has.
    pub const INVALID: Self = Self {
        index: u32::MAX,
        generation: u32::MAX,
    };

    /// Slot index (built-ins use the reserved indices `0..builtin::COUNT`).
    pub fn index(self) -> u32 {
        self.index
    }

    pub(crate) const fn reserved(index: u32) -> Self {
        Self { index, generation: 0 }
    }
}

/// Why an effect was rejected.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EffectError {
    /// Malformed RON.
    #[error("RON: {0}")]
    Parse(String),
    /// A layer breaks a limit.
    #[error("layer {layer:?}: {reason}")]
    Invalid {
        /// Layer (or effect) name.
        layer: String,
        /// What is wrong.
        reason: String,
    },
    /// The handle is stale or was never issued.
    #[error("unknown effect handle")]
    UnknownHandle,
}

struct Slot {
    generation: u32,
    effect: Option<ParticleEffect>,
    /// Reserved for an effect asset still loading: emitters wait for it.
    pending: bool,
}

/// Registered effects (resource, inserted by [`crate::ParticlesPlugin`]).
#[derive(Default)]
pub struct ParticleEffects {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// Bumped by every change (the runtime and the render node rebuild what depends on it).
    version: u64,
    /// Effects that follow an asset (`add_asset`).
    #[cfg(feature = "assets")]
    pub(crate) asset_links: Vec<(space3d_assets::Handle<ParticleEffect>, EffectHandle)>,
}

impl std::fmt::Debug for ParticleEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.iter().map(|(_, e)| e.name.as_str()))
            .finish()
    }
}

impl ParticleEffects {
    /// Register an effect (validated first; the procedural built-in textures it uses are
    /// generated now, once per process, so no render frame pays for them).
    pub fn add(&mut self, effect: ParticleEffect) -> Result<EffectHandle, EffectError> {
        effect.validate()?;
        crate::render::textures::prepare_effect(&effect);
        // Reserved built-in slots are never handed out for user effects.
        while self.slots.len() < builtin::COUNT as usize {
            self.slots.push(Slot {
                generation: 0,
                effect: None,
                pending: false,
            });
        }
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    effect: None,
                    pending: false,
                });
                self.slots.len() as u32 - 1
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.effect = Some(effect);
        slot.pending = false;
        self.version += 1;
        Ok(EffectHandle {
            index,
            generation: slot.generation,
        })
    }

    /// Register the `.pfx.ron` effect asset `asset` (`AssetServer::load`): its handle is valid at
    /// once, emitters playing it wait until it has loaded, and it follows the asset's hot reloads
    /// like [`Self::set`]. An asset that fails to load plays nothing (emitters finish, warned).
    #[cfg(feature = "assets")]
    pub fn add_asset(&mut self, asset: space3d_assets::Handle<ParticleEffect>) -> EffectHandle {
        if let Some(&(_, h)) = self.asset_links.iter().find(|(a, _)| a.id() == asset.id()) {
            return h;
        }
        let h = self
            .add(ParticleEffect::default())
            .expect("the default effect is valid");
        let slot = &mut self.slots[h.index as usize];
        slot.effect = None;
        slot.pending = true;
        self.asset_links.push((asset, h));
        h
    }

    /// Whether `handle` is an effect asset still loading (emitters wait for it).
    pub fn is_pending(&self, handle: EffectHandle) -> bool {
        self.slots
            .get(handle.index as usize)
            .is_some_and(|s| s.generation == handle.generation && s.pending)
    }

    /// An effect asset could not be loaded: its emitters play nothing.
    #[cfg(feature = "assets")]
    pub(crate) fn fail_pending(&mut self, handle: EffectHandle) {
        if let Some(slot) = self.slots.get_mut(handle.index as usize)
            && slot.generation == handle.generation
        {
            slot.pending = false;
            self.version += 1;
        }
    }

    /// Register `effect` at a built-in handle.
    pub(crate) fn set_builtin(&mut self, handle: EffectHandle, effect: ParticleEffect) {
        crate::render::textures::prepare_effect(&effect);
        while self.slots.len() <= handle.index as usize {
            self.slots.push(Slot {
                generation: 0,
                effect: None,
                pending: false,
            });
        }
        self.slots[handle.index as usize].effect = Some(effect);
        self.version += 1;
    }

    /// Replace an effect (hot reload). Render parameters and curves apply next frame; spawn and
    /// simulation parameters apply to new particles.
    pub fn set(&mut self, handle: EffectHandle, effect: ParticleEffect) -> Result<(), EffectError> {
        effect.validate()?;
        crate::render::textures::prepare_effect(&effect);
        let slot = self
            .slots
            .get_mut(handle.index as usize)
            .filter(|s| s.generation == handle.generation && (s.effect.is_some() || s.pending))
            .ok_or(EffectError::UnknownHandle)?;
        slot.effect = Some(effect);
        slot.pending = false;
        self.version += 1;
        Ok(())
    }

    /// Unregister an effect (running instances drain).
    pub fn remove(&mut self, handle: EffectHandle) -> Option<ParticleEffect> {
        #[cfg(feature = "assets")]
        self.asset_links.retain(|&(_, h)| h != handle);
        let slot = self
            .slots
            .get_mut(handle.index as usize)
            .filter(|s| s.generation == handle.generation && (s.effect.is_some() || s.pending))?;
        slot.pending = false;
        let effect = slot.effect.take().unwrap_or_default();
        slot.generation = slot.generation.wrapping_add(1);
        self.version += 1;
        if handle.index >= builtin::COUNT {
            self.free.push(handle.index);
        }
        Some(effect)
    }

    /// The effect of `handle`.
    pub fn get(&self, handle: EffectHandle) -> Option<&ParticleEffect> {
        let slot = self.slots.get(handle.index as usize)?;
        (slot.generation == handle.generation)
            .then_some(slot.effect.as_ref())
            .flatten()
    }

    /// The handle of the effect named `name`.
    pub fn find(&self, name: &str) -> Option<EffectHandle> {
        self.iter().find(|(_, e)| e.name == name).map(|(h, _)| h)
    }

    /// Every effect with its handle, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (EffectHandle, &ParticleEffect)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
            s.effect.as_ref().map(|e| {
                (
                    EffectHandle {
                        index: i as u32,
                        generation: s.generation,
                    },
                    e,
                )
            })
        })
    }

    /// Number of effects.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.effect.is_some()).count()
    }

    /// No effects registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Changes whenever an effect is added, replaced or removed.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// Number of slots (the highest handle index + 1).
    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

/// The built-in effect library: handles reserved at indices `0..COUNT` whenever
/// `ParticlesPlugin::builtins` is set. The effects are RON data (`assets/particles/*.pfx.ron`,
/// embedded; [`SOURCES`](builtin::SOURCES)) authored for HDR + bloom, scale 1 ≈ a 40-unit hull.
pub mod builtin {
    use super::EffectHandle;
    use crate::effect::ParticleEffect;

    /// `"space3d/explosion"`.
    pub const EXPLOSION: EffectHandle = EffectHandle::reserved(0);
    /// `"space3d/hit_spark"`.
    pub const HIT_SPARK: EffectHandle = EffectHandle::reserved(1);
    /// `"space3d/shield_ripple"`.
    pub const SHIELD_RIPPLE: EffectHandle = EffectHandle::reserved(2);
    /// `"space3d/warp_in"`.
    pub const WARP_IN: EffectHandle = EffectHandle::reserved(3);
    /// `"space3d/warp_out"`.
    pub const WARP_OUT: EffectHandle = EffectHandle::reserved(4);
    /// `"space3d/engine_exhaust"` (looping).
    pub const ENGINE_EXHAUST: EffectHandle = EffectHandle::reserved(5);
    /// `"space3d/laser_trail"` (a bolt's glowing wake + head glow; follows a bolt).
    pub const LASER_TRAIL: EffectHandle = EffectHandle::reserved(6);
    /// `"space3d/laser_beam"` (a continuous laser to its `ParticleBeam` end; looping).
    pub const LASER_BEAM: EffectHandle = EffectHandle::reserved(7);
    /// `"space3d/missile_trail"` (flame, wake and a smoke ribbon; follows a missile; looping).
    pub const MISSILE_TRAIL: EffectHandle = EffectHandle::reserved(8);
    /// `"space3d/debris_sparks"` (tumbling hot chunks, sparks, embers and a light flash).
    pub const DEBRIS_SPARKS: EffectHandle = EffectHandle::reserved(9);
    /// `"space3d/engine_afterburner"` (a boosting engine: long plume, shock diamonds, a ribbon
    /// trail; looping).
    pub const ENGINE_AFTERBURNER: EffectHandle = EffectHandle::reserved(10);
    /// `"space3d/emp"` (an electromagnetic pulse: flash, shock rings, arcs, a light flash).
    pub const EMP: EffectHandle = EffectHandle::reserved(11);
    /// `"space3d/repair_beam"` (a flowing green beam to its `ParticleBeam` end; looping).
    pub const REPAIR_BEAM: EffectHandle = EffectHandle::reserved(12);
    /// `"space3d/laser_shot"` (a one-shot hitscan beam to its `ParticleBeam` end, 0.12 s).
    pub const LASER_SHOT: EffectHandle = EffectHandle::reserved(13);

    /// Every built-in, in handle order.
    pub const ALL: [EffectHandle; 14] = [
        EXPLOSION,
        HIT_SPARK,
        SHIELD_RIPPLE,
        WARP_IN,
        WARP_OUT,
        ENGINE_EXHAUST,
        LASER_TRAIL,
        LASER_BEAM,
        MISSILE_TRAIL,
        DEBRIS_SPARKS,
        ENGINE_AFTERBURNER,
        EMP,
        REPAIR_BEAM,
        LASER_SHOT,
    ];

    /// Reserved built-in slots.
    pub(crate) const COUNT: u32 = ALL.len() as u32;

    /// `(name, RON source)` of every built-in, in handle order (also shipped as files under
    /// `crates/space3d-particles/assets/particles/`).
    pub const SOURCES: &[(&str, &str)] = &[
        (
            "space3d/explosion",
            include_str!("../assets/particles/explosion.pfx.ron"),
        ),
        (
            "space3d/hit_spark",
            include_str!("../assets/particles/hit_spark.pfx.ron"),
        ),
        (
            "space3d/shield_ripple",
            include_str!("../assets/particles/shield_ripple.pfx.ron"),
        ),
        ("space3d/warp_in", include_str!("../assets/particles/warp_in.pfx.ron")),
        ("space3d/warp_out", include_str!("../assets/particles/warp_out.pfx.ron")),
        (
            "space3d/engine_exhaust",
            include_str!("../assets/particles/engine_exhaust.pfx.ron"),
        ),
        (
            "space3d/laser_trail",
            include_str!("../assets/particles/laser_trail.pfx.ron"),
        ),
        (
            "space3d/laser_beam",
            include_str!("../assets/particles/laser_beam.pfx.ron"),
        ),
        (
            "space3d/missile_trail",
            include_str!("../assets/particles/missile_trail.pfx.ron"),
        ),
        (
            "space3d/debris_sparks",
            include_str!("../assets/particles/debris_sparks.pfx.ron"),
        ),
        (
            "space3d/engine_afterburner",
            include_str!("../assets/particles/engine_afterburner.pfx.ron"),
        ),
        ("space3d/emp", include_str!("../assets/particles/emp.pfx.ron")),
        (
            "space3d/repair_beam",
            include_str!("../assets/particles/repair_beam.pfx.ron"),
        ),
        (
            "space3d/laser_shot",
            include_str!("../assets/particles/laser_shot.pfx.ron"),
        ),
    ];

    /// The built-in effect of `handle`, parsed from [`SOURCES`] (`None` for other handles).
    pub fn effect(handle: EffectHandle) -> Option<ParticleEffect> {
        if handle.generation != 0 {
            return None;
        }
        let (_, src) = SOURCES.get(handle.index() as usize)?;
        match ParticleEffect::from_ron(src) {
            Ok(e) => Some(e),
            Err(err) => {
                tracing::error!("built-in particle effect {handle:?}: {err}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HDR calibration against `Bloom::glow()` (threshold 1, knee from 0.5; story
    /// `Engine/Particles/Features/HDR Bloom`): every built-in has a layer at 4 or more of peak
    /// radiance (a core, a flash, sparks, a filament: it blooms and reads near white), and none of its smoke
    /// reaches 1 (smoke never blooms). Peak radiance: the layer's intensity × its brightest
    /// color channel over life × its brightest start color, or its white core, untinted.
    #[test]
    fn builtins_are_calibrated_for_bloom() {
        use crate::curve::Gradient;
        use crate::effect::{SpriteShape, SpriteTarget};
        let peak = |g: &Gradient, intensity: f32| {
            g.keys.iter().map(|(_, c)| c[0].max(c[1]).max(c[2])).fold(0.0, f32::max) * intensity
        };
        for h in builtin::ALL {
            let fx = builtin::effect(h).unwrap();
            let mut hottest = 0.0f32;
            for l in &fx.layers {
                if !matches!(l.render.target, SpriteTarget::Color) {
                    continue;
                }
                let start = l
                    .start_color
                    .a
                    .iter()
                    .chain(&l.start_color.b)
                    .take(3)
                    .fold(0.0f32, |m, &c| m.max(c));
                // A glow's or streak's white-hot core adds `core` × intensity at its center.
                let core = match l.render.shape {
                    SpriteShape::Glow { core, .. } | SpriteShape::Streak { core, .. } => core * l.intensity,
                    _ => 0.0,
                };
                let p = (peak(&l.color_over_life, l.intensity) * start.max(1.0)).max(core);
                hottest = hottest.max(p);
                let smoke = matches!(l.render.shape, SpriteShape::Smoke { .. })
                    && (l.name.contains("smoke") || l.name.contains("puffs"));
                assert!(!smoke || p < 1.0, "{} {}: smoke at {p}", fx.name, l.name);
            }
            for r in &fx.ribbons {
                hottest = hottest.max(peak(&r.color_over_life, r.intensity));
            }
            for b in &fx.beams {
                hottest = hottest.max(peak(&b.color, b.intensity));
            }
            assert!(hottest >= 4.0, "{}: hottest layer {hottest}", fx.name);
        }
    }

    #[test]
    fn registry_add_set_remove() {
        let mut fx = ParticleEffects::default();
        assert!(fx.is_empty());
        let a = fx.add(ParticleEffect::new("a")).unwrap();
        assert!(a.index() >= builtin::COUNT, "built-in slots are reserved");
        assert_eq!(fx.find("a"), Some(a));
        assert!(fx.set(a, ParticleEffect::new("a2")).is_ok());
        assert_eq!(fx.get(a).unwrap().name, "a2");
        assert_eq!(fx.remove(a).unwrap().name, "a2");
        assert!(fx.get(a).is_none() && fx.remove(a).is_none());
        assert_eq!(fx.set(a, ParticleEffect::new("x")), Err(EffectError::UnknownHandle));
        let b = fx.add(ParticleEffect::new("b")).unwrap();
        assert_eq!(b.index(), a.index(), "slot reused");
        assert_ne!(b, a, "with a new generation");
        fx.set_builtin(builtin::EXPLOSION, ParticleEffect::new("space3d/explosion"));
        assert_eq!(fx.find("space3d/explosion"), Some(builtin::EXPLOSION));
        assert_eq!(fx.len(), 2);
        assert!(builtin::effect(builtin::LASER_BEAM).is_some());
        assert!(builtin::effect(EffectHandle::reserved(builtin::COUNT)).is_none());
        assert!(fx.get(EffectHandle::INVALID).is_none());
        let v = fx.version();
        fx.remove(b);
        assert!(fx.version() > v, "every change bumps the version");
    }

    /// Every built-in parses, validates, round-trips through RON and sits at its handle.
    #[test]
    fn builtins_parse_and_round_trip() {
        assert_eq!(builtin::SOURCES.len(), builtin::ALL.len());
        for (h, (name, src)) in builtin::ALL.into_iter().zip(builtin::SOURCES) {
            let e = ParticleEffect::from_ron(src).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(&e.name, name);
            assert!(
                !e.layers.is_empty() || !e.beams.is_empty() || !e.ribbons.is_empty(),
                "{name}"
            );
            assert_eq!(builtin::effect(h).as_ref(), Some(&e));
            assert_eq!(ParticleEffect::from_ron(&e.to_ron()).unwrap(), e, "{name} round-trips");
        }
    }
}
