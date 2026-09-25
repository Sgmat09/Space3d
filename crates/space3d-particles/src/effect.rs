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
            if r.max_points > MAX_RIBBON_POINTS {
                return Err(EffectError::Invalid {
                    layer: r.name.clone(),
                    reason: format!("{} ribbon points (at most {MAX_RIBBON_POINTS})", r.max_points),
                });
            }
        }
        Ok(())
    }
    /// Longest particle or ribbon point life after the effect starts, delays included.
    pub fn max_lifetime(&self) -> f32 {
        let sprites = self
            .layers
            .iter()
            .map(|l| l.start + l.particle_delay.max + l.lifetime.max);
        let ribbons = self.ribbons.iter().map(|r| r.start + r.lifetime);
        sprites.chain(ribbons).fold(0.0, f32::max)
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
    /// Override the computed ring size.
    pub capacity: Option<u32>,
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
    /// Particles per world unit the emitter moves.
    pub rate_per_distance: f32,
    /// Upper bound of `ParticleEmitter::rate_scale`. Default 1.
    pub max_rate_scale: f32,
    /// Bursts.
    pub bursts: Vec<Burst>,
}

impl Default for Spawn {
    fn default() -> Self {
        Self {
            rate: 0.0,
            rate_per_distance: 0.0,
            max_rate_scale: 1.0,
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
    /// The post distortion buffer (`DISTORTION_WGSL`); only post cameras show it.
    Distortion {
        /// Offset at full alpha, in view heights.
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
    /// An asset path.
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
    /// Particles back to front (GPU sort).
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
    /// 0..1 over the whole length.
    #[default]
    Stretch,
    /// Repeats every `length` world units.
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

/// A trail following the emitter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RibbonLayer {
    /// Name.
    pub name: String,
    /// Delay after the effect starts.
    pub start: f32,
    /// Local offset.
    pub offset: Vec3,
    /// History ring. Default 64.
    pub max_points: u32,
    /// New point after moving this far …
    pub min_distance: f32,
    /// … or after this long (0 = every tick).
    pub max_interval: f32,
    /// Point lifetime (trail length in time).
    pub lifetime: f32,
    /// Width.
    pub width: f32,
    /// Width over life.
    pub width_over_life: Curve,
    /// Color over life.
    pub color_over_life: Gradient,
    /// HDR multiplier.
    pub intensity: f32,
    /// Texture.
    pub texture: Option<SpriteTexture>,
    /// Analytic cross-section.
    pub profile: RibbonProfile,
    /// UVs along the ribbon.
    pub uv: RibbonUv,
    /// U per second.
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
        }
    }
}

/// A beam (laser) from the emitter to `ParticleBeam::end` (or `length` along −Z).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BeamLayer {
    /// Name.
    pub name: String,
    /// Width.
    pub width: f32,
    /// Length without a `ParticleBeam`.
    pub length: f32,
    /// Color along the beam.
    pub color: Gradient,
    /// Fade over effect time (one-shot tracers).
    pub fade: Curve,
    /// Segments.
    pub segments: u32,
    /// Lightning-style wobble amplitude.
    pub jitter: f32,
    /// Wobble frequency.
    pub jitter_hz: f32,
    /// HDR multiplier.
    pub intensity: f32,
    /// Texture.
    pub texture: Option<SpriteTexture>,
    /// Analytic cross-section.
    pub profile: RibbonProfile,
    /// UVs along the beam.
    pub uv: RibbonUv,
    /// U per second.
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
        }
    }
}

/// A point light flash (an ordinary `PointLight` while it plays).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LightFlash {
    /// Delay after the effect starts.
    pub start: f32,
    /// Seconds.
    pub duration: f32,
    /// Linear color.
    pub color: Vec3,
    /// Peak intensity.
    pub peak: f32,
    /// Intensity over the flash (× peak).
    pub intensity: Curve,
    /// Light range.
    pub range: f32,
    /// Local offset.
    pub offset: Vec3,
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
