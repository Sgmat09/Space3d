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
}

/// Registered effects (resource, inserted by [`crate::ParticlesPlugin`]).
#[derive(Default)]
pub struct ParticleEffects {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl std::fmt::Debug for ParticleEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.iter().map(|(_, e)| e.name.as_str()))
            .finish()
    }
}

impl ParticleEffects {
    /// Register an effect (validated first).
    pub fn add(&mut self, effect: ParticleEffect) -> Result<EffectHandle, EffectError> {
        effect.validate()?;
        // Reserved built-in slots are never handed out for user effects.
        while self.slots.len() < builtin::COUNT as usize {
            self.slots.push(Slot {
                generation: 0,
                effect: None,
            });
        }
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    effect: None,
                });
                self.slots.len() as u32 - 1
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.effect = Some(effect);
        Ok(EffectHandle {
            index,
            generation: slot.generation,
        })
    }

    /// Register `effect` at a built-in handle.
    pub(crate) fn set_builtin(&mut self, handle: EffectHandle, effect: ParticleEffect) {
        while self.slots.len() <= handle.index as usize {
            self.slots.push(Slot {
                generation: 0,
                effect: None,
            });
        }
        self.slots[handle.index as usize].effect = Some(effect);
    }

    /// Replace an effect (hot reload). Render parameters and curves apply next frame; spawn and
    /// simulation parameters apply to new particles.
    pub fn set(&mut self, handle: EffectHandle, effect: ParticleEffect) -> Result<(), EffectError> {
        effect.validate()?;
        let slot = self.slot_mut(handle).ok_or(EffectError::UnknownHandle)?;
        slot.effect = Some(effect);
        Ok(())
    }

    /// Unregister an effect (running instances drain).
    pub fn remove(&mut self, handle: EffectHandle) -> Option<ParticleEffect> {
        let slot = self.slot_mut(handle)?;
        let effect = slot.effect.take()?;
        slot.generation = slot.generation.wrapping_add(1);
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

    fn slot_mut(&mut self, handle: EffectHandle) -> Option<&mut Slot> {
        self.slots
            .get_mut(handle.index as usize)
            .filter(|s| s.generation == handle.generation && s.effect.is_some())
    }
}

/// The built-in effect library: handles reserved at indices `0..COUNT` whenever
/// `ParticlesPlugin::builtins` is set. Their data lands with the particles milestones PA1/PA3;
/// until then [`builtin::effect`] returns `None` and the handles resolve to nothing.
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
    /// `"space3d/laser_trail"` (ribbon + head glow; follows a bolt).
    pub const LASER_TRAIL: EffectHandle = EffectHandle::reserved(6);
    /// `"space3d/laser_beam"` (looping beam; with `ParticleBeam`).
    pub const LASER_BEAM: EffectHandle = EffectHandle::reserved(7);

    /// Reserved built-in slots.
    pub(crate) const COUNT: u32 = 8;

    /// `(name, RON source)` of every built-in (also shipped as files under
    /// `assets/particles/`).
    pub const SOURCES: &[(&str, &str)] = &[]; // STUB(P-PART): the library (PA1, PA3)

    /// The built-in effect of `handle`, parsed from [`SOURCES`].
    pub fn effect(handle: EffectHandle) -> Option<ParticleEffect> {
        let (_, src) = SOURCES.get(handle.index() as usize)?;
        ParticleEffect::from_ron(src).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(builtin::effect(builtin::LASER_BEAM).is_none());
        assert!(fx.get(EffectHandle::INVALID).is_none());
    }
}
