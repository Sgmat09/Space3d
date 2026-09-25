//! Render layers: one per sprite layer of every registered effect — its draw parameters
//! (`GpuDrawParams`, a dynamic-offset uniform) and its two rows of the curve LUT (color
//! gradient; size, alpha and coverage curves), rebuilt only when the effects change.

use crate::cpu::PackLayer;
use crate::effect::{Facing, LUT_WIDTH, ParticleBlend, SortMode, SpriteShape};
use crate::pack::f16_bits;
use crate::registry::{EffectHandle, ParticleEffects};

/// Draw parameters of a render layer (`S3dpDraw` in `sprite.wgsl`, 80 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuDrawParams {
    pub lut: [f32; 4],
    pub mode: [u32; 4],
    pub shape: [f32; 4],
    pub stretch: [f32; 4],
    pub fade: [f32; 4],
}

/// Sorting facts of a render layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayerInfo {
    pub pack: PackLayer,
    /// `SpriteRender::order`.
    pub order: i16,
    /// Additive layers draw after alpha and premultiplied ones of the same order.
    pub additive: bool,
}

/// Every render layer, in effect slot order.
#[derive(Debug, Default)]
pub(crate) struct RenderLayers {
    /// `ParticleEffects::version` these were built from (`None`: never built).
    pub version: Option<u64>,
    /// First render layer of each effect slot, and its sprite layer count.
    base: Vec<(u32, u16, EffectHandle)>,
    pub params: Vec<GpuDrawParams>,
    pub info: Vec<LayerInfo>,
    /// `Rgba16Float` texels, `LUT_WIDTH` × `rows`.
    pub lut: Vec<u16>,
    pub rows: u32,
    warned_texture: bool,
}

fn shape_params(shape: &SpriteShape) -> (u32, [f32; 4], bool) {
    match *shape {
        SpriteShape::Glow { core, hardness } => (0, [core, hardness.clamp(0.0, 1.0), 0.0, 0.0], false),
        SpriteShape::Streak { core, taper } => (1, [core, taper.clamp(0.0, 1.0), 0.0, 0.0], false),
        SpriteShape::Ring { thickness, focus, fill } => (2, [thickness, focus.max(0.0), fill, 0.0], false),
        SpriteShape::Smoke { breakup, hot_core } => (3, [breakup, hot_core.clamp(0.0, 1.0), 0.0, 0.0], false),
        SpriteShape::Vortex { arms, twist } => (4, [arms, twist, 0.0, 0.0], false),
        // Textures and flipbooks land with milestone PA3: a soft glow until then.
        SpriteShape::Texture(_) => (0, [0.0; 4], true),
    }
}

impl RenderLayers {
    /// Rebuild from `effects` if they changed; returns whether anything was rebuilt.
    pub fn sync(&mut self, effects: &ParticleEffects) -> bool {
        if self.version == Some(effects.version()) {
            return false;
        }
        self.version = Some(effects.version());
        self.base.clear();
        self.params.clear();
        self.info.clear();
        self.lut.clear();
        self.base
            .resize(effects.slot_count(), (u32::MAX, 0, EffectHandle::INVALID));
        let total: usize = effects.iter().map(|(_, e)| e.layers.len()).sum();
        self.rows = (total as u32 * 2).max(2);
        for (handle, effect) in effects.iter() {
            let first = self.params.len() as u32;
            self.base[handle.index() as usize] = (first, effect.layers.len() as u16, handle);
            for layer in &effect.layers {
                let id = self.params.len() as u32;
                let r = &layer.render;
                let (kind, shape, texture) = shape_params(&r.shape);
                if texture && !self.warned_texture {
                    self.warned_texture = true;
                    tracing::warn!(
                        effect = %effect.name,
                        "particle sprite textures and flipbooks arrive with milestone PA3; drawing a soft glow"
                    );
                }
                let (facing, stretch) = match r.facing {
                    Facing::Camera => (0, [0.0, 1.0, 1.0]),
                    Facing::VelocityStretched { scale, min, max } => {
                        (1, [scale.max(0.0), min.max(0.0), max.max(min.max(0.0))])
                    }
                    Facing::Plane { .. } | Facing::EmitterPlane => (2, [0.0, 1.0, 1.0]),
                };
                let blend = match r.blend {
                    ParticleBlend::Additive => 0,
                    ParticleBlend::Alpha => 1,
                    ParticleBlend::Premultiplied => 2,
                };
                let row = |r: u32| (r as f32 + 0.5) / self.rows as f32;
                self.params.push(GpuDrawParams {
                    lut: [row(2 * id), row(2 * id + 1), layer.intensity, layer.aspect.max(1e-3)],
                    mode: [blend, facing, kind, 0],
                    shape,
                    stretch: [stretch[0], stretch[1], stretch[2], r.soft_distance.max(0.0)],
                    fade: [r.near_fade.0, r.near_fade.1, 0.0, 0.0],
                });
                let extent = 0.5 * layer.size_over_life.max_value().max(0.0) * layer.aspect.max(1.0).max(stretch[2]);
                let additive = r.blend == ParticleBlend::Additive;
                let batched = additive && matches!(r.sort, SortMode::Auto | SortMode::None);
                self.info.push(LayerInfo {
                    pack: PackLayer { id, batched, extent },
                    order: r.order,
                    additive,
                });
                // Color row, then (size, alpha, coverage, 0).
                for i in 0..LUT_WIDTH {
                    let t = i as f32 / (LUT_WIDTH - 1) as f32;
                    let c = layer.color_over_life.eval(t);
                    self.lut.extend(c.to_array().map(f16_bits));
                }
                for i in 0..LUT_WIDTH {
                    let t = i as f32 / (LUT_WIDTH - 1) as f32;
                    self.lut.extend(
                        [
                            layer.size_over_life.eval(t),
                            layer.alpha_over_life.eval(t),
                            layer.coverage_over_life.eval(t),
                            0.0,
                        ]
                        .map(f16_bits),
                    );
                }
            }
        }
        let texels = (LUT_WIDTH * self.rows * 4) as usize;
        self.lut.resize(texels, 0);
        true
    }

    /// The render layer of sprite layer `layer` of `effect` (`None` if the effect is gone or
    /// changed shape).
    pub fn layer(&self, effect: EffectHandle, layer: u16) -> Option<&LayerInfo> {
        let &(first, count, handle) = self.base.get(effect.index() as usize)?;
        if handle != effect || layer >= count {
            return None;
        }
        self.info.get((first + layer as u32) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{Curve, FadeCurve, Gradient};
    use crate::effect::{ParticleEffect, ParticleLayer, SpriteRender};
    use crate::pack::f16_value;
    use space3d_core::glam::Vec4;

    #[test]
    fn lut_rows_hold_the_curves_at_texel_centers() {
        let mut fx = ParticleEffects::default();
        let layer = ParticleLayer {
            color_over_life: Gradient::linear(Vec4::new(4.0, 0.0, 0.0, 1.0), Vec4::new(0.0, 0.0, 2.0, 0.0)),
            size_over_life: Curve::Linear(1.0, 3.0),
            alpha_over_life: Curve::Preset(FadeCurve::Out),
            coverage_over_life: Curve::Constant(0.5),
            render: SpriteRender {
                blend: ParticleBlend::Premultiplied,
                order: -2,
                ..SpriteRender::default()
            },
            ..ParticleLayer::default()
        };
        let a = fx
            .add(ParticleEffect::new("a").layer(ParticleLayer::default()))
            .unwrap();
        let b = fx.add(ParticleEffect::new("b").layer(layer)).unwrap();
        let mut l = RenderLayers::default();
        assert!(l.sync(&fx));
        assert!(!l.sync(&fx), "unchanged");
        assert_eq!(l.rows, 4);
        assert_eq!(l.lut.len(), (LUT_WIDTH * 4 * 4) as usize);
        let info = *l.layer(b, 0).unwrap();
        assert_eq!(
            (info.pack.id, info.order, info.additive, info.pack.batched),
            (1, -2, false, false)
        );
        assert!(l.layer(a, 0).unwrap().pack.batched, "unsorted additive layers batch");
        assert!(l.layer(b, 1).is_none() && l.layer(EffectHandle::INVALID, 0).is_none());
        let texel = |row: u32, i: u32| {
            let o = ((row * LUT_WIDTH + i) * 4) as usize;
            [0, 1, 2, 3].map(|c| f16_value(l.lut[o + c]))
        };
        assert_eq!(texel(2, 0), [4.0, 0.0, 0.0, 1.0]);
        assert_eq!(texel(2, LUT_WIDTH - 1), [0.0, 0.0, 2.0, 0.0]);
        assert_eq!(texel(3, 0), [1.0, 1.0, 0.5, 0.0]);
        assert_eq!(texel(3, LUT_WIDTH - 1), [3.0, 0.0, 0.5, 0.0]);
        let p = l.params[1];
        assert_eq!((p.lut[0], p.lut[1]), (2.5 / 4.0, 3.5 / 4.0));
        assert_eq!(p.mode[0], 2);
        assert_eq!(info.pack.extent, 1.5, "size curve max 3 × ½");
        fx.remove(a);
        assert!(l.sync(&fx));
        assert!(l.layer(a, 0).is_none(), "removed effects draw nothing");
    }
}
