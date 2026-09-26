//! The effect data model (design-particles §4.4 with the graphics spec §2.5 deltas): an effect
//! is sprite layers, ribbon layers, beam layers and an optional light flash. Every struct has
//! `#[serde(default)]` and a `Default`, so RON files list only non-default fields.

use crate::curve::{ColorRange, CountRange, Curve, FloatRange, Gradient};
use crate::registry::EffectError;
use serde::{Deserialize, Serialize};
use space3d_core::glam::Vec3;
use space3d_render::{BlendMode, TextureHandle};

/// Most sprite + ribbon + beam layers per effect.
pub const MAX_LAYERS_PER_EFFECT: usize = 16;
/// Most forces per layer.
pub const MAX_FORCES_PER_LAYER: usize = 4;
/// Samples of every baked curve and gradient.
pub const LUT_WIDTH: u32 = 128;
/// Most flipbook frames.
pub const MAX_FLIPBOOK_FRAMES: u32 = 256;
/// Most points of a ribbon's history.
pub const MAX_RIBBON_POINTS: u32 = 1024;
/// Most segments of a beam.
pub const MAX_BEAM_SEGMENTS: u32 = 64;

/// A particle effect: what a [`crate::ParticleEmitter`] plays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParticleEffect {
    /// Name (registry lookups, scene serialization).
    pub name: String,
    /// Emission window of non-looping effects, in seconds.
    pub duration: f32,
    /// Emit forever.
    pub looping: bool,
    /// Sprite particles.
    pub layers: Vec<ParticleLayer>,
    /// Trails.
    pub ribbons: Vec<RibbonLayer>,
    /// Lasers.
    pub beams: Vec<BeamLayer>,
    /// A point light flash.
    pub light: Option<LightFlash>,
    /// Who gets refused first when the particle budget runs out.
    pub priority: EffectPriority,
    /// Default velocity smoothing (seconds, 0: off) of the emitters playing this effect whose
    /// `ParticleEmitter::velocity_smoothing` is `None`: plumes and trails of emitters that may
    /// move in steps (ships easing toward server updates) set about 0.1. See
    /// [`crate::ParticleEmitter::velocity_smoothing`].
    pub velocity_smoothing: f32,
}

impl Default for ParticleEffect {
    fn default() -> Self {
        Self {
            name: String::new(),
            duration: 1.0,
            looping: false,
            layers: Vec::new(),
            ribbons: Vec::new(),
            beams: Vec::new(),
            light: None,
            priority: EffectPriority::Normal,
            velocity_smoothing: 0.0,
        }
    }
}

impl ParticleEffect {
    /// An empty effect named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
    /// Add a sprite layer.
    pub fn layer(mut self, l: ParticleLayer) -> Self {
        self.layers.push(l);
        self
    }
    /// Add a ribbon layer.
    pub fn ribbon(mut self, r: RibbonLayer) -> Self {
        self.ribbons.push(r);
        self
    }
    /// Add a beam layer.
    pub fn beam(mut self, b: BeamLayer) -> Self {
        self.beams.push(b);
        self
    }
    /// Parse an effect from RON.
    pub fn from_ron(src: &str) -> Result<Self, EffectError> {
        let effect: Self = ron::from_str(src).map_err(|e| EffectError::Parse(e.to_string()))?;
        effect.validate()?;
        Ok(effect)
    }
    /// The effect as pretty RON.
    pub fn to_ron(&self) -> String {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).unwrap_or_default()
    }
    /// Check the limits (`MAX_LAYERS_PER_EFFECT`, `MAX_FORCES_PER_LAYER`, …).
    pub fn validate(&self) -> Result<(), EffectError> {
        let layers = self.layers.len() + self.ribbons.len() + self.beams.len();
        if layers > MAX_LAYERS_PER_EFFECT {
            return Err(EffectError::Invalid {
                layer: self.name.clone(),
                reason: format!("{layers} layers (at most {MAX_LAYERS_PER_EFFECT})"),
            });
        }
        for l in &self.layers {
            if l.forces.len() > MAX_FORCES_PER_LAYER {
                return Err(EffectError::Invalid {
                    layer: l.name.clone(),
                    reason: format!("{} forces (at most {MAX_FORCES_PER_LAYER})", l.forces.len()),
                });
            }
            if l.lifetime.min < 0.0 || l.lifetime.max < l.lifetime.min {
                return Err(EffectError::Invalid {
                    layer: l.name.clone(),
                    reason: "lifetime must be a non-negative range".into(),
                });
            }
        }
        for r in &self.ribbons {
            if !(2..=MAX_RIBBON_POINTS).contains(&r.max_points) {
                return Err(EffectError::Invalid {
                    layer: r.name.clone(),
                    reason: format!("{} ribbon points (2 to {MAX_RIBBON_POINTS})", r.max_points),
                });
            }
            if r.lifetime.is_nan() || r.lifetime <= 0.0 {
                return Err(EffectError::Invalid {
                    layer: r.name.clone(),
                    reason: "a ribbon's point lifetime must be positive".into(),
                });
            }
        }
        for b in &self.beams {
            if !(1..=MAX_BEAM_SEGMENTS).contains(&b.segments) {
                return Err(EffectError::Invalid {
                    layer: b.name.clone(),
                    reason: format!("{} beam segments (1 to {MAX_BEAM_SEGMENTS})", b.segments),
                });
            }
        }
        for l in &self.layers {
            if let Some(f) = &l.render.flipbook
                && (f.frames == 0 || f.frames > MAX_FLIPBOOK_FRAMES || f.columns == 0 || f.rows == 0)
            {
                return Err(EffectError::Invalid {
                    layer: l.name.clone(),
                    reason: format!(
                        "flipbook of {} frames in {}×{} cells (1 to {MAX_FLIPBOOK_FRAMES} frames, at least one cell)",
                        f.frames, f.columns, f.rows
                    ),
                });
            }
        }
        Ok(())
    }
    /// Longest particle or ribbon point life after the effect starts, delays included (and the
    /// light flash's end).
    pub fn max_lifetime(&self) -> f32 {
        let sprites = self
            .layers
            .iter()
            .map(|l| l.start + l.particle_delay.max + l.lifetime.max);
        let ribbons = self.ribbons.iter().map(|r| r.start + r.lifetime);
        let light = self.light.iter().map(|f| f.start + f.duration);
        sprites.chain(ribbons).chain(light).fold(0.0, f32::max)
    }
}

/// Budget priority of an effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EffectPriority {
    /// First to be refused or shrunk.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Shrunk only after `Normal`.
    High,
    /// Never refused (shrunk last).
    Critical,
}

/// A layer of sprite particles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParticleLayer {
    /// Name (errors, stats).
    pub name: String,
    /// Default true.
    pub enabled: bool,
    /// Delay after the effect starts, in seconds.
    pub start: f32,
    /// Emission.
    pub spawn: Spawn,
    /// Emission shape. Default `Point`.
    pub shape: EmitShape,
    /// Initial direction. Default `Shape`.
    pub direction: EmitDirection,
    /// Random cone half-angle around the direction (radians).
    pub spread: f32,
    /// Local offset from the emitter.
    pub offset: Vec3,
    /// 0..1: squash offsets and directions along `flatten_axis`.
    pub flatten: f32,
    /// Default +Y (a top-down flight plane).
    pub flatten_axis: Vec3,
    /// Simulation space. Default `World`.
    pub space: SimulationSpace,
    /// Lifetime in seconds. Default 1.
    pub lifetime: FloatRange,
    /// Per-particle appearance delay.
    pub particle_delay: FloatRange,
    /// Initial speed.
    pub speed: FloatRange,
    /// Speeds × emitter scale^k. Default 1.
    pub speed_scale_exponent: f32,
    /// × emitter velocity. Default 0.
    pub inherit_velocity: f32,
    /// 1/s, exact exponential.
    pub drag: f32,
    /// In `space`.
    pub gravity: Vec3,
    /// At most `MAX_FORCES_PER_LAYER`.
    pub forces: Vec<Force>,
    /// `ParticleAttractor` mask (0 = none).
    pub world_forces: u32,
    /// Start diameter in world units. Default 1.
    pub size: FloatRange,
    /// Size multiplier over life. Default constant 1.
    pub size_over_life: Curve,
    /// Width / height. Default 1.
    pub aspect: f32,
    /// Initial rotation (radians).
    pub rotation: FloatRange,
    /// Radians per second.
    pub angular_velocity: FloatRange,
    /// Randomly mirror sprites.
    pub random_mirror: bool,
    /// Linear HDR RGBA over life.
    pub color_over_life: Gradient,
    /// Random lerp between two colors per particle.
    pub start_color: ColorRange,
    /// Brightness / opacity multiplier over life.
    pub alpha_over_life: Curve,
    /// Premultiplied coverage (Premultiplied blend only).
    pub coverage: FloatRange,
    /// Coverage over life.
    pub coverage_over_life: Curve,
    /// HDR multiplier. Default 1.
    pub intensity: f32,
    /// Brightness flicker with a per-particle phase.
    pub flicker: Option<Flicker>,
    /// How the particles are drawn.
    pub render: SpriteRender,
    /// The emitter tint applies. Default true.
    pub tinted: bool,
    /// Quality / LOD may reduce counts. Default true.
    pub lod: bool,
    /// Override the computed ring size: the most particles of one layer instance alive at
    /// once (older ones are overwritten). Computed by default from the rates at
    /// `max_rate_scale`, the bursts and the life window, with `rate_per_distance` taken at
    /// [`Spawn::max_speed`] (100 units/s unless set).
    pub capacity: Option<u32>,
    /// How `ParticleEmitter::rate_scale` (the throttle) drives the layer: by default only its
    /// rates, linearly.
    pub throttle: ThrottleResponse,
}

/// How a layer responds to its emitter's throttle `t = ParticleEmitter::rate_scale` (clamped to
/// `Spawn::max_rate_scale`): rates × `t^rate`, initial speeds × `t^speed`, sprite sizes ×
/// `t^size`, brightness × `t^intensity`. The default (1, 0, 0, 0) scales only the rates. A
/// flame reads better with a slowly thinning density and a plume that shortens and dims
/// instead (`ENGINE_EXHAUST`: `(rate: 0.25, speed: 0.6, size: 0.3, intensity: 0.5)`), so it
/// never breaks into puffs at partial throttle. At `t = 0` nothing is emitted, whatever the
/// exponents; sizes and brightness apply to live particles at once (the flame responds
/// instantly), speeds to new ones.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThrottleResponse {
    /// Exponent of the rates' response. Default 1 (linear).
    pub rate: f32,
    /// Exponent of the initial speeds' response. Default 0 (none).
    pub speed: f32,
    /// Exponent of the sprite sizes' response. Default 0 (none).
    pub size: f32,
    /// Exponent of the brightness response. Default 0 (none).
    pub intensity: f32,
}

impl Default for ThrottleResponse {
    fn default() -> Self {
        Self {
            rate: 1.0,
            speed: 0.0,
            size: 0.0,
            intensity: 0.0,
        }
    }
}

impl ThrottleResponse {
    /// `t^exponent` for a throttle `t` ≥ 0 (1 for a zero exponent, including at `t = 0`: sizes
    /// and brightness of layers that do not respond stay put).
    pub(crate) fn factor(t: f32, exponent: f32) -> f32 {
        if exponent == 0.0 {
            1.0
        } else if exponent == 1.0 {
            t.max(0.0)
        } else {
            t.max(0.0).powf(exponent)
        }
    }
}

impl Default for ParticleLayer {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            start: 0.0,
            spawn: Spawn::default(),
            shape: EmitShape::Point,
            direction: EmitDirection::Shape,
            spread: 0.0,
            offset: Vec3::ZERO,
            flatten: 0.0,
            flatten_axis: Vec3::Y,
            space: SimulationSpace::World,
            lifetime: FloatRange::constant(1.0),
            particle_delay: FloatRange::constant(0.0),
            speed: FloatRange::constant(0.0),
            speed_scale_exponent: 1.0,
            inherit_velocity: 0.0,
            drag: 0.0,
            gravity: Vec3::ZERO,
            forces: Vec::new(),
            world_forces: 0,
            size: FloatRange::constant(1.0),
            size_over_life: Curve::default(),
            aspect: 1.0,
            rotation: FloatRange::constant(0.0),
            angular_velocity: FloatRange::constant(0.0),
            random_mirror: false,
            color_over_life: Gradient::default(),
            start_color: ColorRange::default(),
            alpha_over_life: Curve::default(),
            coverage: FloatRange::constant(1.0),
            coverage_over_life: Curve::default(),
            intensity: 1.0,
            flicker: None,
            render: SpriteRender::default(),
            tinted: true,
            lod: true,
            capacity: None,
            throttle: ThrottleResponse::default(),
        }
    }
}

impl ParticleLayer {
    /// A layer bursting `count` particles at its start, living `lifetime` seconds.
    pub fn burst(count: impl Into<CountRange>, lifetime: impl Into<FloatRange>) -> Self {
        Self {
            spawn: Spawn {
                bursts: vec![Burst {
                    count: count.into(),
                    ..Burst::default()
                }],
                ..Spawn::default()
            },
            lifetime: lifetime.into(),
            ..Self::default()
        }
    }
    /// A layer emitting `rate` particles per second, living `lifetime` seconds.
    pub fn continuous(rate: f32, lifetime: impl Into<FloatRange>) -> Self {
        Self {
            spawn: Spawn {
                rate,
                ..Spawn::default()
            },
            lifetime: lifetime.into(),
            ..Self::default()
        }
    }
    /// Set [`Self::name`].
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
    /// Set [`Self::shape`].
    pub fn shape(mut self, s: EmitShape) -> Self {
        self.shape = s;
        self
    }
    /// Set [`Self::direction`].
    pub fn direction(mut self, d: EmitDirection) -> Self {
        self.direction = d;
        self
    }
    /// Set [`Self::speed`].
    pub fn speed(mut self, s: impl Into<FloatRange>) -> Self {
        self.speed = s.into();
        self
    }
    /// Set [`Self::size`].
    pub fn size(mut self, s: impl Into<FloatRange>) -> Self {
        self.size = s.into();
        self
    }
    /// Set [`Self::drag`].
    pub fn drag(mut self, d: f32) -> Self {
        self.drag = d;
        self
    }
    /// Set [`Self::gravity`].
    pub fn gravity(mut self, g: Vec3) -> Self {
        self.gravity = g;
        self
    }
    /// Set [`Self::color_over_life`].
    pub fn color(mut self, g: Gradient) -> Self {
        self.color_over_life = g;
        self
    }
    /// Set [`Self::intensity`].
    pub fn intensity(mut self, i: f32) -> Self {
        self.intensity = i;
        self
    }
    /// Set [`Self::render`].
    pub fn render(mut self, r: SpriteRender) -> Self {
        self.render = r;
        self
    }
}

/// Emission of a sprite layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spawn {
    /// Particles per second.
    pub rate: f32,
    /// Particles per world unit the emitter moves. The layer's ring is sized for an emitter
    /// flying at [`Self::max_speed`]; see there.
    pub rate_per_distance: f32,
    /// Upper bound of `ParticleEmitter::rate_scale`. Default 1.
    pub max_rate_scale: f32,
    /// Fastest emitter speed (world units per second) the ring of a `rate_per_distance` layer
    /// is sized for: `rate_per_distance × max_rate_scale × max_speed × life` particles.
    /// Default 100. A **looping** effect whose emitter keeps flying faster (three ticks in a
    /// row) restarts with rings for its speed (the next power-of-two multiple of `max_speed`
    /// covering 1.25 × the speed, up to 64×; the old particles drain, as for a LOD change); a
    /// **one-shot** effect cannot, and a faster emitter wraps its ring: the oldest particles
    /// of the trail disappear early. Set it (or `ParticleLayer::capacity`) for trails on fast
    /// bolts and afterburning ships.
    pub max_speed: f32,
    /// Bursts.
    pub bursts: Vec<Burst>,
}

impl Default for Spawn {
    fn default() -> Self {
        Self {
            rate: 0.0,
            rate_per_distance: 0.0,
            max_rate_scale: 1.0,
            max_speed: 100.0,
            bursts: Vec::new(),
        }
    }
}

/// A burst of particles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Burst {
    /// Seconds after the layer starts.
    pub time: f32,
    /// Particles per cycle.
    pub count: CountRange,
    /// Repetitions (0 = forever). Default 1.
    pub cycles: u32,
    /// Seconds between cycles.
    pub interval: f32,
}

impl Default for Burst {
    fn default() -> Self {
        Self {
            time: 0.0,
            count: CountRange::from(1),
            cycles: 1,
            interval: 0.0,
        }
    }
}

/// Where particles appear (emitter-local).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EmitShape {
    /// The emitter's position.
    #[default]
    Point,
    /// A ball; `inner` 0 = volume … 1 = surface.
    Sphere {
        /// Radius.
        radius: f32,
        /// Inner radius fraction.
        inner: f32,
    },
    /// Around local −Z (the emitter's forward), from a base disc.
    Cone {
        /// Half-angle in radians.
        angle: f32,
        /// Base disc radius.
        radius: f32,
    },
    /// A ring in the local XZ plane (normal +Y); `arc` from +X.
    Ring {
        /// Radius.
        radius: f32,
        /// Radial width.
        width: f32,
        /// Arc in radians.
        arc: f32,
    },
    /// A box.
    Box {
        /// Half extents.
        half_extents: Vec3,
    },
    /// A segment along local X, centred.
    Line {
        /// Length.
        length: f32,
    },
}

/// Initial direction of particles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EmitDirection {
    /// Given by the shape (outward, cone axis, …).
    #[default]
    Shape,
    /// A fixed local direction.
    Fixed(Vec3),
    /// Uniformly random.
    Random,
    /// Toward the emitter.
    Inward,
    /// Tangent to the shape.
    Tangent,
}

/// Simulation space of a layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SimulationSpace {
    /// Particles stay where they were emitted.
    #[default]
    World,
    /// Particles move with the emitter.
    Local,
}

/// A force on a layer's particles.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Force {
    /// Toward an emitter-local point.
    Attractor {
        /// Point.
        position: Vec3,
        /// Strength.
        strength: f32,
        /// Radius of influence.
        radius: f32,
        /// Falloff.
        falloff: Falloff,
    },
    /// Around an axis.
    Vortex {
        /// Axis.
        axis: Vec3,
        /// Tangential strength.
        strength: f32,
        /// Pull toward the axis.
        pull: f32,
    },
    /// Hash value-noise turbulence.
    Turbulence {
        /// Strength.
        strength: f32,
        /// Spatial frequency.
        frequency: f32,
        /// Time scroll speed.
        speed: f32,
    },
}

/// Force falloff with distance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Falloff {
    /// None.
    #[default]
    Constant,
    /// Linear to 0 at the radius.
    Linear,
    /// 1 / d².
    InverseSquare,
}

/// Brightness flicker.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Flicker {
    /// 0..1.
    pub amplitude: f32,
    /// Hz.
    pub frequency: f32,
}

impl Default for Flicker {
    fn default() -> Self {
        Self {
            amplitude: 0.3,
            frequency: 8.0,
        }
    }
}

/// How a sprite layer is drawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpriteRender {
    /// Analytic shape or texture. Default `Glow { core: 0, hardness: 0 }`.
    pub shape: SpriteShape,
    /// Flipbook animation of a texture.
    pub flipbook: Option<Flipbook>,
    /// Blend. Default `Additive`.
    pub blend: ParticleBlend,
    /// Orientation. Default `Camera`.
    pub facing: Facing,
    /// World units; 0 = hard depth test.
    pub soft_distance: f32,
    /// Camera distance: invisible at `.0`, full at `.1` (`(0, 0)` = off).
    pub near_fade: (f32, f32),
    /// Sorting. Default `Auto`.
    pub sort: SortMode,
    /// Draw layer within the particles node (lower first). Independent of
    /// `Material::render_order`, which orders meshes only.
    pub order: i16,
    /// Color or distortion. Default `Color`.
    pub target: SpriteTarget,
    /// Largest on-screen size of a sprite, as a fraction of the viewport height; 0 (the
    /// default) uses `ParticleSettings::max_screen_size`. For intentionally huge sprites (a
    /// warp vortex) whose size and timing must not depend on the camera's zoom.
    pub max_screen_size: f32,
}

impl Default for SpriteRender {
    fn default() -> Self {
        Self {
            shape: SpriteShape::Glow {
                core: 0.0,
                hardness: 0.0,
            },
            flipbook: None,
            blend: ParticleBlend::Additive,
            facing: Facing::Camera,
            soft_distance: 0.0,
            near_fade: (0.0, 0.0),
            sort: SortMode::Auto,
            order: 0,
            target: SpriteTarget::Color,
            max_screen_size: 0.0,
        }
    }
}

impl SpriteRender {
    /// Additive sprites of `shape`.
    pub fn additive(shape: SpriteShape) -> Self {
        Self {
            shape,
            blend: ParticleBlend::Additive,
            ..Self::default()
        }
    }
    /// Velocity-stretched sprites (`scale` seconds of velocity, clamped 1..8 × the size).
    pub fn stretched(mut self, scale: f32) -> Self {
        self.facing = Facing::VelocityStretched {
            scale,
            min: 1.0,
            max: 8.0,
        };
        self
    }
    /// Draw into the post distortion buffer instead of color.
    pub fn distortion(mut self, strength: f32) -> Self {
        self.target = SpriteTarget::Distortion { strength };
        self
    }
}

/// Where sprites draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SpriteTarget {
    /// Scene color.
    #[default]
    Color,
    /// The post group's distortion buffer (post's writer API, `DISTORTION_WGSL`): the layer
    /// bends the image behind it instead of drawing light, seen only by cameras whose
    /// `PostProcess` enables distortion. The shape gives the direction: `Ring` pushes the image
    /// under its band outward (a shockwave), `Vortex` swirls it, a texture offsets it by its RG
    /// channels (0.5 = none), the other shapes shimmer it like heat; the weight is the shape ×
    /// the opacity over life × the near fade, faded against scene depth by `soft_distance`.
    Distortion {
        /// Offset at full weight, in view heights (0.01 = 1 % of the view's height), at most
        /// 0.12 × the sprite's on-screen radius for heat haze (`Glow`, `Streak`, `Smoke`) and
        /// 0.5 × for `Ring`, `Vortex` and textures: a sprite never bends the image by more than
        /// a fraction of its own size, so small, distant or top-down writers shimmer instead of
        /// shifting what they cover (an engine plume under its own haze) by their width.
        strength: f32,
    },
}

/// Analytic sprite shapes and textures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SpriteShape {
    /// A texture.
    Texture(SpriteTexture),
    /// A soft round glow.
    Glow {
        /// Solid core fraction.
        core: f32,
        /// Edge hardness.
        hardness: f32,
    },
    /// A capsule (pair with `Facing::VelocityStretched`).
    Streak {
        /// Solid core fraction.
        core: f32,
        /// Tail taper.
        taper: f32,
    },
    /// A ring; `focus` sharpens it toward local +X (rotation).
    Ring {
        /// Thickness.
        thickness: f32,
        /// Focus.
        focus: f32,
        /// Inner fill.
        fill: f32,
    },
    /// A noisy puff with a per-particle phase.
    Smoke {
        /// Breakup.
        breakup: f32,
        /// Hot core.
        hot_core: f32,
    },
    /// A spiral.
    Vortex {
        /// Arms.
        arms: f32,
        /// Twist.
        twist: f32,
    },
}

/// A sprite texture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SpriteTexture {
    /// A procedural built-in.
    Builtin(BuiltinTexture),
    /// An asset path (the asset server's root, or `source://path`): with the `assets` feature
    /// and an `AssetServer`, `ParticlesPlugin` loads the image, uploads it (sRGB; linear for a
    /// distortion layer) and swaps it into the effect as [`Self::Handle`]; the layer draws
    /// untextured until then. Without them the path is warned about once and never drawn.
    Path(String),
    /// A registered texture (not serialized).
    #[serde(skip)]
    Handle(TextureHandle),
}

/// Procedural textures generated at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BuiltinTexture {
    /// 16-frame fireball flipbook.
    Fireball16,
    /// 16-frame smoke flipbook.
    Smoke16,
    /// A sparkle.
    Sparkle,
    /// A soft dot.
    SoftDot,
}

/// Flipbook animation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Flipbook {
    /// Frames (≤ `MAX_FLIPBOOK_FRAMES`).
    pub frames: u32,
    /// Atlas columns.
    pub columns: u32,
    /// Atlas rows.
    pub rows: u32,
    /// Frame selection.
    pub mode: FlipbookMode,
    /// Blend between frames.
    pub blend: bool,
}

impl Default for Flipbook {
    fn default() -> Self {
        Self {
            frames: 1,
            columns: 1,
            rows: 1,
            mode: FlipbookMode::OverLife { cycles: 1.0 },
            blend: false,
        }
    }
}

/// Flipbook frame selection.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FlipbookMode {
    /// `cycles` loops over the life.
    OverLife {
        /// Loops.
        cycles: f32,
    },
    /// Frames per second.
    Fps(f32),
    /// A random frame per particle.
    Random,
    /// A random start frame, then frames per second.
    RandomStartFps(f32),
}

/// Particle blend (the equations match `BlendMode`'s, tested for parity).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ParticleBlend {
    /// `dst + src × alpha`.
    #[default]
    Additive,
    /// Alpha blending.
    Alpha,
    /// Premultiplied alpha (alpha 0 adds light).
    Premultiplied,
}

impl ParticleBlend {
    /// The particle blend of a material blend mode (`None` for opaque modes).
    pub fn from_blend_mode(mode: BlendMode) -> Option<Self> {
        match mode {
            BlendMode::Additive => Some(Self::Additive),
            BlendMode::AlphaBlend => Some(Self::Alpha),
            BlendMode::Premultiplied => Some(Self::Premultiplied),
            _ => None,
        }
    }
}

/// Sprite orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Facing {
    /// Screen-aligned, rotated in the screen plane.
    #[default]
    Camera,
    /// Stretched along the velocity.
    VelocityStretched {
        /// Seconds of velocity.
        scale: f32,
        /// Minimum stretch.
        min: f32,
        /// Maximum stretch.
        max: f32,
    },
    /// In a plane of `space` (top-down rings: +Y).
    Plane {
        /// Plane normal.
        normal: Vec3,
    },
    /// The emitter's local XZ plane at spawn.
    EmitterPlane,
}

/// Sorting of a layer's particles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SortMode {
    /// `None` for additive, `ByEmitter` otherwise.
    #[default]
    Auto,
    /// Spawn order.
    None,
    /// Emitters back to front.
    ByEmitter,
    /// Particles back to front in every view: the layer's emitters draw as one group whose
    /// particles are sorted by view depth (ties by instance order: deterministic) — on the CPU
    /// for direct draws, with a bitonic sort on the GPU backend (groups of at most 65 536).
    ByDistance,
}

/// Cross-section of ribbons and beams.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RibbonProfile {
    /// Solid core fraction.
    pub core: f32,
    /// Edge hardness.
    pub hardness: f32,
}

impl Default for RibbonProfile {
    fn default() -> Self {
        Self {
            core: 0.2,
            hardness: 0.0,
        }
    }
}

/// Texture coordinates along ribbons and beams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RibbonUv {
    /// 0..1 over the whole length (a ribbon's `u` is its points' age / lifetime: 0 at the head).
    #[default]
    Stretch,
    /// Repeats every `length` world units (× effect scale for beams: world units), fixed to the
    /// path: a ribbon's texture stays where it was laid.
    Tile {
        /// Tile length.
        length: f32,
    },
}

/// Ribbon and beam orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RibbonFacing {
    /// Facing the camera.
    #[default]
    Camera,
    /// In a plane.
    Plane {
        /// Plane normal.
        normal: Vec3,
    },
}

/// A trail following the emitter: a history of the emitter's positions (a point per
/// `min_distance` moved, or per `max_interval`), each living `lifetime` seconds, drawn as one
/// continuous strip — watertight at any bend (neighboring segments share each point's side
/// vector) — from its oldest live point, whose end is clipped to exactly `lifetime` so the tail
/// shrinks smoothly, to the emitter's render-time position (the head, which follows the emitter
/// at any frame rate and whichever stage moves it). Width and color over the points' life come
/// from `width_over_life` and `color_over_life`; `u` runs along the trail (`uv`, `scroll`); the
/// head end is round. Built on the CPU on both backends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RibbonLayer {
    /// Name.
    pub name: String,
    /// Delay after the effect starts.
    pub start: f32,
    /// Local offset.
    pub offset: Vec3,
    /// History ring (2 to `MAX_RIBBON_POINTS`). Default 64.
    pub max_points: u32,
    /// A new point once the emitter moved this far from the newest (× effect scale) …
    pub min_distance: f32,
    /// … or once this long passed since it (0: by distance only).
    pub max_interval: f32,
    /// Point lifetime in seconds (the trail's length in time).
    pub lifetime: f32,
    /// Width in world units (× effect scale).
    pub width: f32,
    /// Width multiplier over a point's life.
    pub width_over_life: Curve,
    /// Linear HDR color over a point's life (alpha: opacity).
    pub color_over_life: Gradient,
    /// HDR multiplier.
    pub intensity: f32,
    /// Texture (multiplies the color; `u` along, `v` across).
    pub texture: Option<SpriteTexture>,
    /// Analytic cross-section.
    pub profile: RibbonProfile,
    /// UVs along the ribbon (`u` grows toward the head).
    pub uv: RibbonUv,
    /// The texture and noise move toward the head at this many units of `u` per second.
    pub scroll: f32,
    /// Orientation.
    pub facing: RibbonFacing,
    /// Blend.
    pub blend: ParticleBlend,
    /// Soft depth fade distance.
    pub soft_distance: f32,
    /// The emitter tint applies (each point keeps the tint of its time).
    pub tinted: bool,
    /// Draw order within the particles node.
    pub order: i16,
    /// 0..1: breaks the strip up with value noise scrolling along it (smoke, plasma).
    pub noise: f32,
}

impl Default for RibbonLayer {
    fn default() -> Self {
        Self {
            name: String::new(),
            start: 0.0,
            offset: Vec3::ZERO,
            max_points: 64,
            min_distance: 0.1,
            max_interval: 0.0,
            lifetime: 0.5,
            width: 0.1,
            width_over_life: Curve::default(),
            color_over_life: Gradient::default(),
            intensity: 1.0,
            texture: None,
            profile: RibbonProfile::default(),
            uv: RibbonUv::Stretch,
            scroll: 0.0,
            facing: RibbonFacing::Camera,
            blend: ParticleBlend::Additive,
            soft_distance: 0.0,
            tinted: true,
            order: 0,
            noise: 0.0,
        }
    }
}

/// A beam (laser) from the emitter to `ParticleBeam::end` (or `length` along −Z): a strip of
/// `segments` segments rebuilt every frame from the render-time end points (hitscan lasers,
/// tractor and repair beams), with round ends, optional glows at both ends, lightning-style
/// `jitter`, a brightness `flicker` and scrolling `noise`. Visible while the effect emits (a
/// non-looping effect's `duration`, faded by `fade` over it: a one-shot tracer).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BeamLayer {
    /// Name.
    pub name: String,
    /// Width in world units (× effect scale × `ParticleBeam::width_scale`).
    pub width: f32,
    /// Length without a `ParticleBeam` (× effect scale).
    pub length: f32,
    /// Linear HDR color along the beam, start (0) to end (1).
    pub color: Gradient,
    /// Brightness over the effect's time / `duration` (one-shot tracers; looping effects use
    /// its value at 0).
    pub fade: Curve,
    /// Segments (1 to `MAX_BEAM_SEGMENTS`).
    pub segments: u32,
    /// Lightning-style sideways wobble of the inner points, world units (× effect scale); 0 at
    /// the ends.
    pub jitter: f32,
    /// New wobble this many times per second (0: once).
    pub jitter_hz: f32,
    /// HDR multiplier.
    pub intensity: f32,
    /// Texture.
    pub texture: Option<SpriteTexture>,
    /// Analytic cross-section.
    pub profile: RibbonProfile,
    /// UVs along the beam (`u` grows toward the end).
    pub uv: RibbonUv,
    /// The texture and noise flow toward the end at this many units of `u` per second.
    pub scroll: f32,
    /// Orientation.
    pub facing: RibbonFacing,
    /// Blend.
    pub blend: ParticleBlend,
    /// Soft depth fade distance.
    pub soft_distance: f32,
    /// The emitter tint applies.
    pub tinted: bool,
    /// Draw order within the particles node.
    pub order: i16,
    /// Start point offset from the emitter (emitter-local, × effect scale): a muzzle.
    pub offset: Vec3,
    /// Brightness flicker of the whole beam.
    pub flicker: Option<Flicker>,
    /// 0..1: breaks the beam up with value noise scrolling along it (`scroll`).
    pub noise: f32,
    /// Diameter of a glow at the start, in beam widths (0: none).
    pub start_glow: f32,
    /// Diameter of a glow at the end (an impact), in beam widths (0: none).
    pub end_glow: f32,
}

impl Default for BeamLayer {
    fn default() -> Self {
        Self {
            name: String::new(),
            width: 0.1,
            length: 10.0,
            color: Gradient::default(),
            fade: Curve::default(),
            segments: 1,
            jitter: 0.0,
            jitter_hz: 0.0,
            intensity: 1.0,
            texture: None,
            profile: RibbonProfile::default(),
            uv: RibbonUv::Stretch,
            scroll: 0.0,
            facing: RibbonFacing::Camera,
            blend: ParticleBlend::Additive,
            soft_distance: 0.0,
            tinted: true,
            order: 0,
            offset: Vec3::ZERO,
            flicker: None,
            noise: 0.0,
            start_glow: 0.0,
            end_glow: 0.0,
        }
    }
}

/// A point light flash: while it plays, a child entity of the emitter carries an ordinary
/// `PointLight` (so it lights meshes, shadows aside, and counts toward the renderer's point
/// lights: the 16 nearest per view beyond 16); it is despawned when the flash ends, and the
/// effect finishes only after it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightFlash {
    /// Delay after the effect starts.
    pub start: f32,
    /// Seconds.
    pub duration: f32,
    /// Linear color.
    pub color: Vec3,
    /// Peak intensity (`PointLight::intensity`, × effect scale²: the same light at the same
    /// distance relative to the effect's size). The renderer's point lights fall off with the
    /// inverse square of the distance: a flash lighting a hull 20–40 units away wants
    /// hundreds to thousands.
    pub peak: f32,
    /// Intensity over the flash (× peak).
    pub intensity: Curve,
    /// Light range (× effect scale).
    pub range: f32,
    /// Local offset (× effect scale).
    pub offset: Vec3,
    /// The emitter tint's color applies. Default false.
    pub tinted: bool,
}

impl Default for LightFlash {
    fn default() -> Self {
        Self {
            start: 0.0,
            duration: 0.2,
            color: Vec3::ONE,
            peak: 4.0,
            intensity: Curve::Preset(crate::curve::FadeCurve::Flash),
            range: 10.0,
            offset: Vec3::ZERO,
            tinted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ron_round_trip_lists_only_non_defaults() {
        let e = ParticleEffect::new("spark")
            .layer(
                ParticleLayer::burst((8, 16), (0.3, 0.6))
                    .named("sparks")
                    .shape(EmitShape::Sphere {
                        radius: 0.5,
                        inner: 1.0,
                    })
                    .speed((5.0, 12.0))
                    .drag(2.0)
                    .render(SpriteRender::additive(SpriteShape::Streak { core: 0.2, taper: 1.0 }).stretched(0.04)),
            )
            .ribbon(RibbonLayer::default())
            .beam(BeamLayer::default());
        let src = e.to_ron();
        assert_eq!(ParticleEffect::from_ron(&src).unwrap(), e);
        let minimal = ParticleEffect::from_ron("(name: \"x\", layers: [(lifetime: (0.5, 1.0))])").unwrap();
        assert_eq!(minimal.layers[0].lifetime, FloatRange::new(0.5, 1.0));
        assert_eq!(minimal.layers[0].render, SpriteRender::default());
        assert_eq!(minimal.max_lifetime(), 1.0);
        assert!(matches!(
            ParticleEffect::from_ron("(layers: 3)"),
            Err(EffectError::Parse(_))
        ));
        let too_many = ParticleEffect {
            layers: vec![ParticleLayer::default(); MAX_LAYERS_PER_EFFECT + 1],
            ..Default::default()
        };
        assert!(matches!(too_many.validate(), Err(EffectError::Invalid { .. })));
    }

    #[test]
    fn blends_follow_material_blend_modes() {
        assert_eq!(
            ParticleBlend::from_blend_mode(BlendMode::Additive),
            Some(ParticleBlend::Additive)
        );
        assert_eq!(
            ParticleBlend::from_blend_mode(BlendMode::AlphaBlend),
            Some(ParticleBlend::Alpha)
        );
        assert_eq!(ParticleBlend::from_blend_mode(BlendMode::Opaque), None);
        assert_eq!(
            SpriteRender::default().distortion(0.02).target,
            SpriteTarget::Distortion { strength: 0.02 }
        );
    }
}
