//! Levels of detail: simpler meshes for objects that are small on screen.
//!
//! A crowd of models a few dozen pixels tall costs the GPU their full triangle count, in the
//! scene pass and again in every shadow map. [`simplify`] makes a coarser mesh from a detailed
//! one, [`MeshLod`] lists an entity's meshes from the most detailed down, and the renderer
//! picks one each frame from the size of the entity's bounding sphere on screen
//! ([`LodSettings`]): the largest it has in any active camera, so a model shown close up in a
//! preview keeps its detail while the same model far away in the world does not.
//!
//! ```ignore
//! let (coarse, extras) = lod::simplify_to(&data, &extras, 0.25);
//! let low = assets.add_mesh_with(coarse, extras);
//! world.spawn((Transform::default(), Mesh3d(full), material, MeshLod::new(radius, &[full, low])));
//! ```

use crate::assets::RenderAssets;
use crate::components::{Camera3d, Mesh3d, Projection, RenderTarget};
use crate::culling::max_axis_scale;
use crate::handles::MeshHandle;
use crate::material::MeshExtras;
use crate::renderer::Renderer;
use glam::Vec3;
use space3d_core::{GlobalTransform, MeshData, Resources, World};
use std::collections::{HashMap, HashSet};

/// The most levels a [`MeshLod`] holds, the full mesh included.
pub const MAX_LOD_LEVELS: usize = 4;

/// The levels of detail of an entity's mesh. The renderer sets the entity's [`Mesh3d`] to one
/// of `meshes` each frame; an entity without this component keeps the mesh it has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshLod {
    meshes: [MeshHandle; MAX_LOD_LEVELS],
    count: u8,
    /// Radius of the sphere that is measured on screen, in the entity's own units (it is
    /// scaled by the entity's `GlobalTransform`). For a model of several parts give every part
    /// the whole model's radius, so the parts change level together.
    pub radius: f32,
    /// Where the sphere's centre is, in the entity's own units.
    pub center: Vec3,
}

impl MeshLod {
    /// `meshes` from the most detailed (the full mesh) down, at most [`MAX_LOD_LEVELS`] (the
    /// rest is left out); `radius` as [`Self::radius`].
    ///
    /// # Panics
    /// With no mesh at all.
    pub fn new(radius: f32, meshes: &[MeshHandle]) -> Self {
        assert!(!meshes.is_empty(), "a MeshLod needs its full mesh");
        let count = meshes.len().min(MAX_LOD_LEVELS);
        let mut all = [meshes[0]; MAX_LOD_LEVELS];
        all[..count].copy_from_slice(&meshes[..count]);
        Self {
            meshes: all,
            count: count as u8,
            radius,
            center: Vec3::ZERO,
        }
    }

    /// With the measured sphere's centre moved (a part whose own origin is not the model's).
    pub fn with_center(mut self, center: Vec3) -> Self {
        self.center = center;
        self
    }

    /// The meshes, most detailed first.
    pub fn meshes(&self) -> &[MeshHandle] {
        &self.meshes[..self.count as usize]
    }

    /// Which level `mesh` is (0: the full mesh); `None` for a mesh that is not one of them.
    pub fn level_of(&self, mesh: MeshHandle) -> Option<usize> {
        self.meshes().iter().position(|m| *m == mesh)
    }
}

/// How levels of detail are picked (resource; the defaults without it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodSettings {
    /// Off: every [`MeshLod`] entity draws its full mesh.
    pub enabled: bool,
    /// Level `i + 1` is drawn while the bounding sphere's radius on screen is under
    /// `pixels[i]` physical pixels (descending). The default keeps the full mesh above 56 px,
    /// then a second level down to 20 px and a third below.
    pub pixels: [f32; MAX_LOD_LEVELS - 1],
    /// Multiplies `pixels`: above 1 simpler meshes are used sooner (a low quality tier),
    /// below 1 later.
    pub bias: f32,
}

impl Default for LodSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            pixels: [56.0, 20.0, 0.0],
            bias: 1.0,
        }
    }
}

/// A level changes only once the size is this far past its threshold, so an object whose size
/// sits on a threshold does not flicker between two meshes.
const HYSTERESIS: f32 = 0.1;

impl LodSettings {
    /// The level for a sphere `px` pixels in radius, ignoring the level it has now.
    fn level_for(&self, px: f32) -> usize {
        self.pixels.iter().filter(|t| px < **t * self.bias).count()
    }

    /// The level for a sphere `px` pixels in radius that is drawn at `current` now.
    pub fn pick(&self, px: f32, current: usize) -> usize {
        if !self.enabled {
            return 0;
        }
        let finest = self.level_for(px * (1.0 + HYSTERESIS));
        let coarsest = self.level_for(px * (1.0 - HYSTERESIS));
        current.clamp(finest, coarsest)
    }
}

/// How a camera sizes a sphere on screen.
#[derive(Clone, Copy)]
enum Scale {
    /// Pixels = radius × this ÷ distance from the eye.
    Perspective(Vec3, f32),
    /// Pixels = radius × this.
    Orthographic(f32),
}

/// More active cameras than this and every entity keeps its full mesh (it does not happen in
/// practice; the scratch stays on the stack).
const MAX_CAMERAS: usize = 16;

/// Set every [`MeshLod`] entity's [`Mesh3d`] to the level its size on screen asks for. Runs in
/// the Render stage before the frame is extracted; allocates nothing.
pub(crate) fn select_lods(world: &mut World, resources: &Resources) {
    let settings = resources.get::<LodSettings>().copied().unwrap_or_default();
    let primary = resources.get::<Renderer>().map_or((1280, 720), Renderer::size);
    let assets = resources.get::<RenderAssets>();
    let mut cameras = [Scale::Orthographic(0.0); MAX_CAMERAS];
    let mut count = 0;
    let mut overflow = false;
    for (camera, at) in world.query_mut::<(&Camera3d, &GlobalTransform)>() {
        if !camera.active {
            continue;
        }
        if count == MAX_CAMERAS {
            overflow = true;
            break;
        }
        let target_height = match camera.target {
            RenderTarget::Primary => primary.1,
            RenderTarget::Texture(t) => assets.and_then(|a| a.texture_size(t)).map_or(primary.1, |s| s.1),
        } as f32;
        let height = target_height * camera.viewport.map_or(1.0, |v| v[3]);
        cameras[count] = match camera.projection {
            Projection::Perspective { fov_y, .. } => {
                Scale::Perspective(at.0.w_axis.truncate(), height * 0.5 / (fov_y * 0.5).tan().max(1e-6))
            }
            Projection::Orthographic { height: world, .. } => Scale::Orthographic(height / world.max(1e-6)),
        };
        count += 1;
    }
    let cameras = &cameras[..count];
    let full = !settings.enabled || overflow || cameras.is_empty();
    for (lod, mesh, at) in world.query_mut::<(&MeshLod, &mut Mesh3d, &GlobalTransform)>() {
        let level = if full {
            0
        } else {
            let center = at.0.transform_point3(lod.center);
            let radius = lod.radius * max_axis_scale(&at.0);
            let px = cameras
                .iter()
                .map(|c| match *c {
                    Scale::Perspective(eye, k) => radius * k / eye.distance(center).max(radius.max(1e-6)),
                    Scale::Orthographic(k) => radius * k,
                })
                .fold(0.0, f32::max);
            settings.pick(px, lod.level_of(mesh.0).unwrap_or(0))
        };
        let wanted = lod.meshes[level.min(lod.count as usize - 1)];
        if mesh.0 != wanted {
            mesh.0 = wanted;
        }
    }
}

/// A coarser copy of a mesh by vertex clustering: the vertices inside each cube of side `cell`
/// (in the mesh's units) become one, the one nearest their middle, with the normals of all of
/// them averaged; triangles that collapse are dropped, and so are doubles. Cheap and robust
/// (it needs no manifold mesh), and good for meshes drawn small: seams and hard edges closer
/// than `cell` are welded.
///
/// Vertices are only joined where their texture coordinates are close too (within
/// [`UV_REACH`] cells' worth of texture, by how much texture the mesh lays over a unit of
/// space): the two sides of a seam lie at one place in space and far apart in the texture, and
/// a triangle between vertices of two islands would be painted with whatever lies between them
/// in the atlas.
pub fn simplify(data: &MeshData, extras: &MeshExtras, cell: f32) -> (MeshData, MeshExtras) {
    let n = data.positions.len();
    let (min, _) = data.aabb();
    let inv = 1.0 / cell.max(1e-9);
    let mut ids: HashMap<[i32; 5], u32> = HashMap::with_capacity(n / 4 + 1);
    // How much texture a unit of space is given, over the whole mesh: a cell of space is that much texture across.
    let uv_inv = {
        let (mut space, mut texture) = (0.0f64, 0.0f64);
        if data.uvs.len() == n {
            for t in data.indices.chunks_exact(3) {
                for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                    let (a, b) = (a as usize, b as usize);
                    space += f64::from(Vec3::from(data.positions[a]).distance(Vec3::from(data.positions[b])));
                    texture += f64::from(glam::Vec2::from(data.uvs[a]).distance(glam::Vec2::from(data.uvs[b])));
                }
            }
        }
        let density = if space > 0.0 { (texture / space) as f32 } else { 0.0 };
        if density > 0.0 {
            1.0 / (cell.max(1e-9) * density * UV_REACH)
        } else {
            0.0
        }
    };
    let mut cluster = Vec::with_capacity(n);
    // Per cluster: the sum of its positions and normals, and how many they are.
    let mut sums: Vec<(Vec3, Vec3, u32)> = Vec::new();
    for (i, p) in data.positions.iter().enumerate() {
        let p = Vec3::from(*p);
        let [x, y, z] = ((p - min) * inv).floor().as_ivec3().to_array();
        let [u, v] = data
            .uvs
            .get(i)
            .map_or([0, 0], |uv| uv.map(|c| (c * uv_inv).floor() as i32));
        let key = [x, y, z, u, v];
        let id = *ids.entry(key).or_insert_with(|| {
            sums.push((Vec3::ZERO, Vec3::ZERO, 0));
            sums.len() as u32 - 1
        });
        let s = &mut sums[id as usize];
        s.0 += p;
        s.1 += data.normals.get(i).map_or(Vec3::ZERO, |n| Vec3::from(*n));
        s.2 += 1;
        cluster.push(id);
    }
    // The member nearest each cluster's middle stands for it.
    let mut nearest: Vec<(f32, u32)> = vec![(f32::INFINITY, 0); sums.len()];
    for (i, p) in data.positions.iter().enumerate() {
        let c = cluster[i] as usize;
        let d = Vec3::from(*p).distance_squared(sums[c].0 / sums[c].2 as f32);
        if d < nearest[c].0 {
            nearest[c] = (d, i as u32);
        }
    }
    fn pick<T: Copy>(nearest: &[(f32, u32)], from: &[T], to: &mut Vec<T>) {
        if !from.is_empty() {
            to.extend(nearest.iter().map(|(_, i)| from[*i as usize]));
        }
    }
    let mut out = MeshData::default();
    let mut out_extras = MeshExtras::new();
    pick(&nearest, &data.positions, &mut out.positions);
    pick(&nearest, &data.uvs, &mut out.uvs);
    pick(&nearest, &data.colors, &mut out.colors);
    pick(&nearest, &extras.tangents, &mut out_extras.tangents);
    pick(&nearest, &extras.uvs1, &mut out_extras.uvs1);
    if !data.normals.is_empty() {
        out.normals.extend(nearest.iter().zip(&sums).map(|((_, i), s)| {
            let mean = s.1.normalize_or_zero();
            if mean == Vec3::ZERO {
                data.normals[*i as usize]
            } else {
                mean.to_array()
            }
        }));
    }
    let mut seen: HashSet<[u32; 3]> = HashSet::with_capacity(data.indices.len() / 6 + 1);
    for t in data.indices.chunks_exact(3) {
        let (a, b, c) = (cluster[t[0] as usize], cluster[t[1] as usize], cluster[t[2] as usize]);
        if a == b || b == c || a == c {
            continue;
        }
        let mut key = [a, b, c];
        key.sort_unstable();
        if seen.insert(key) {
            out.indices.extend([a, b, c]);
        }
    }
    (out, out_extras)
}

/// How far apart in the texture [`simplify`] still joins two vertices of one cell of space:
/// this many times the texture a cell of space is across, on average over the mesh.
pub const UV_REACH: f32 = 3.0;

/// [`simplify`] with the smallest cell that leaves at most `ratio` of the mesh's triangles
/// (0.25: a quarter), found by halving. A mesh that cannot be brought that low without
/// vanishing comes back as coarse as it can be; one with no triangles comes back as it is.
pub fn simplify_to(data: &MeshData, extras: &MeshExtras, ratio: f32) -> (MeshData, MeshExtras) {
    let triangles = data.triangle_count();
    let target = ((triangles as f32 * ratio.clamp(0.0, 1.0)) as usize).max(1);
    if triangles <= target {
        return (data.clone(), extras.clone());
    }
    let (min, max) = data.aabb();
    let extent = (max - min).max_element().max(1e-6);
    // Cells from 1/1024 of the mesh (as good as the mesh itself) to half of it, on a log scale
    let cell_at = |t: f32| extent * 2f32.powf(-10.0 + 9.0 * t);
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut best: Option<(MeshData, MeshExtras)> = None;
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        let made = simplify(data, extras, cell_at(mid));
        let count = made.0.triangle_count();
        if count == 0 {
            // too coarse: nothing is left
            hi = mid;
        } else if count <= target {
            best = Some(made);
            hi = mid;
        } else {
            // the coarsest so far that still has triangles, in case the target is never met
            if best.as_ref().is_none_or(|b| b.0.triangle_count() > target) {
                best = Some(made);
            }
            lo = mid;
        }
    }
    best.unwrap_or_else(|| (data.clone(), extras.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::MeshHandle;
    use glam::Mat4;
    use space3d_core::Transform;

    /// A sphere of `rings` by `2 * rings` quads, radius 1, with normals and UVs.
    fn sphere(rings: u32) -> MeshData {
        let mut m = MeshData::default();
        let cols = rings * 2;
        for r in 0..=rings {
            let phi = std::f32::consts::PI * r as f32 / rings as f32;
            for c in 0..=cols {
                let theta = std::f32::consts::TAU * c as f32 / cols as f32;
                let p = Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
                m.positions.push(p.to_array());
                m.normals.push(p.to_array());
                m.uvs.push([c as f32 / cols as f32, r as f32 / rings as f32]);
            }
        }
        for r in 0..rings {
            for c in 0..cols {
                let a = r * (cols + 1) + c;
                let b = a + cols + 1;
                m.indices.extend([a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        m
    }

    #[test]
    fn a_simplified_mesh_is_smaller_and_whole() {
        let full = sphere(48);
        let extras = MeshExtras::new().with_tangents(vec![[1.0, 0.0, 0.0, 1.0]; full.positions.len()]);
        let (coarse, coarse_extras) = simplify(&full, &extras, 0.25);
        assert!(coarse.triangle_count() > 50, "{}", coarse.triangle_count());
        assert!(coarse.triangle_count() * 4 < full.triangle_count());
        // every stream has a value for every vertex, and every index a vertex
        let n = coarse.positions.len();
        assert_eq!(
            (coarse.normals.len(), coarse.uvs.len(), coarse_extras.tangents.len()),
            (n, n, n)
        );
        assert!(coarse.colors.is_empty() && coarse_extras.uvs1.is_empty());
        assert!(coarse.indices.iter().all(|i| (*i as usize) < n));
        // it is still the sphere: vertices on it, normals pointing out, the same bounds within a cell
        for (p, normal) in coarse.positions.iter().zip(&coarse.normals) {
            let (p, normal) = (Vec3::from(*p), Vec3::from(*normal));
            assert!((p.length() - 1.0).abs() < 1e-4);
            assert!(normal.dot(p) > 0.8 && (normal.length() - 1.0).abs() < 1e-3);
        }
        let ((min, max), (full_min, full_max)) = (coarse.aabb(), full.aabb());
        assert!((min - full_min).abs().max_element() < 0.25 && (max - full_max).abs().max_element() < 0.25);
        // no triangle twice, none collapsed
        let mut seen = HashSet::new();
        for t in coarse.indices.chunks_exact(3) {
            assert!(t[0] != t[1] && t[1] != t[2] && t[0] != t[2]);
            let mut key = [t[0], t[1], t[2]];
            key.sort_unstable();
            assert!(seen.insert(key));
        }
    }

    /// Two sheets at one place with their own islands of the texture (a hull's two sides, a seam) are not welded into each other: no
    /// triangle of the simpler mesh reaches from one island to the other.
    #[test]
    fn a_seam_is_not_welded() {
        let mut m = MeshData::default();
        let n = 24u32;
        for (sheet, u0) in [(0u32, 0.0f32), (1, 0.6)] {
            for y in 0..=n {
                for x in 0..=n {
                    let (fx, fy) = (x as f32 / n as f32, y as f32 / n as f32);
                    m.positions.push([fx, fy, 0.0]);
                    m.normals.push([0.0, 0.0, if sheet == 0 { 1.0 } else { -1.0 }]);
                    m.uvs.push([u0 + 0.3 * fx, 0.3 * fy]);
                }
            }
            let base = sheet * (n + 1) * (n + 1);
            for y in 0..n {
                for x in 0..n {
                    let a = base + y * (n + 1) + x;
                    let b = a + n + 1;
                    m.indices.extend([a, b, a + 1, a + 1, b, b + 1]);
                }
            }
        }
        let (coarse, _) = simplify(&m, &MeshExtras::new(), 0.2);
        assert!(coarse.triangle_count() > 20 && coarse.triangle_count() * 4 < m.triangle_count());
        for t in coarse.indices.chunks_exact(3) {
            let u: Vec<f32> = t.iter().map(|i| coarse.uvs[*i as usize][0]).collect();
            let island = |u: f32| u >= 0.45;
            assert!(island(u[0]) == island(u[1]) && island(u[1]) == island(u[2]), "{u:?}");
            // and the two sides keep their own normals
            let z: Vec<f32> = t.iter().map(|i| coarse.normals[*i as usize][2]).collect();
            assert!(
                z.iter().all(|z| z.abs() > 0.99) && z[0] == z[1] && z[1] == z[2],
                "{z:?}"
            );
        }
    }

    #[test]
    fn a_tiny_cell_keeps_the_mesh_and_a_huge_one_nothing() {
        let full = sphere(12);
        let (same, _) = simplify(&full, &MeshExtras::new(), 1e-5);
        // (the seam's and the poles' doubled vertices weld; the triangles at the poles that were slivers go)
        assert!(same.triangle_count() >= full.triangle_count() - 2 * 24);
        let (none, _) = simplify(&full, &MeshExtras::new(), 100.0);
        assert_eq!(none.triangle_count(), 0);
    }

    #[test]
    fn simplify_to_meets_its_ratio() {
        let full = sphere(64);
        for ratio in [0.5, 0.25, 0.06] {
            let (coarse, _) = simplify_to(&full, &MeshExtras::new(), ratio);
            let (got, most) = (coarse.triangle_count(), (full.triangle_count() as f32 * ratio) as usize);
            assert!(got <= most && got * 3 > most, "ratio {ratio}: {got} of at most {most}");
        }
        // nothing to do, and nothing to work with
        assert_eq!(simplify_to(&full, &MeshExtras::new(), 1.0).0.indices, full.indices);
        assert_eq!(
            simplify_to(&MeshData::default(), &MeshExtras::new(), 0.1)
                .0
                .triangle_count(),
            0
        );
        // a single triangle cannot get smaller: it stays
        let mut one = MeshData::default();
        one.positions = vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        one.indices = vec![0, 1, 2];
        assert_eq!(simplify_to(&one, &MeshExtras::new(), 0.1).0.triangle_count(), 1);
    }

    #[test]
    fn a_level_changes_past_its_threshold_only() {
        let s = LodSettings::default();
        assert_eq!((s.pick(200.0, 0), s.pick(40.0, 0), s.pick(5.0, 0)), (0, 1, 2));
        // just under 56 px the full mesh stays, and just over it the second level does
        assert_eq!((s.pick(54.0, 0), s.pick(58.0, 1)), (0, 1));
        assert_eq!((s.pick(49.0, 0), s.pick(63.0, 1)), (1, 0));
        // a bias moves the thresholds; off, it is always the full mesh
        let low = LodSettings { bias: 2.0, ..s };
        assert_eq!((low.pick(100.0, 0), low.pick(30.0, 0)), (1, 2));
        assert_eq!(LodSettings { enabled: false, ..s }.pick(1.0, 2), 0);
    }

    fn handles() -> [MeshHandle; 3] {
        let mut assets = RenderAssets::default();
        [
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::cube(1.0)),
            assets.add_mesh(MeshData::cube(1.0)),
        ]
    }

    #[test]
    fn the_mesh_follows_the_size_on_screen() {
        let [full, mid, low] = handles();
        let mut world = World::new();
        let resources = Resources::default();
        // a camera at the origin looking down -Z, 720 px tall, 60 degrees: a radius-1 sphere at distance d is 624 / d px
        world.spawn((Camera3d::default(), GlobalTransform(Mat4::IDENTITY)));
        let at = |z: f32| GlobalTransform(Transform::from_xyz(0.0, 0.0, z).matrix());
        let lod = MeshLod::new(1.0, &[full, mid, low]);
        let near = world.spawn((lod, Mesh3d(full), at(-5.0)));
        let middle = world.spawn((lod, Mesh3d(full), at(-20.0)));
        let far = world.spawn((lod, Mesh3d(full), at(-100.0)));
        let plain = world.spawn((Mesh3d(full), at(-100.0)));
        // twice the size: the scale counts
        let big = world.spawn((
            lod,
            Mesh3d(full),
            GlobalTransform(Mat4::from_scale_rotation_translation(
                Vec3::splat(4.0),
                glam::Quat::IDENTITY,
                Vec3::new(0.0, 0.0, -20.0),
            )),
        ));
        let mesh = |world: &World, e| world.get::<&Mesh3d>(e).unwrap().0;
        select_lods(&mut world, &resources);
        assert_eq!(
            [near, middle, far, plain, big].map(|e| mesh(&world, e)),
            [full, mid, low, full, full]
        );
        // it comes nearer: finer again
        *world.get::<&mut GlobalTransform>(far).unwrap() = at(-8.0);
        select_lods(&mut world, &resources);
        assert_eq!(mesh(&world, far), full);
        // a second camera right next to the far one keeps it whole
        *world.get::<&mut GlobalTransform>(far).unwrap() = at(-100.0);
        select_lods(&mut world, &resources);
        assert_eq!(mesh(&world, far), low);
        world.spawn((Camera3d::default(), at(-97.0)));
        select_lods(&mut world, &resources);
        assert_eq!(mesh(&world, far), full);
        // switched off: everything whole
        let mut resources = Resources::default();
        resources.insert(LodSettings {
            enabled: false,
            ..LodSettings::default()
        });
        select_lods(&mut world, &resources);
        assert_eq!(mesh(&world, middle), full);
    }

    #[test]
    fn fewer_meshes_than_levels_stop_at_the_last() {
        let [full, mid, _] = handles();
        let mut world = World::new();
        world.spawn((Camera3d::default(), GlobalTransform(Mat4::IDENTITY)));
        let e = world.spawn((
            MeshLod::new(1.0, &[full, mid]),
            Mesh3d(full),
            GlobalTransform(Transform::from_xyz(0.0, 0.0, -500.0).matrix()),
        ));
        select_lods(&mut world, &Resources::default());
        assert_eq!(world.get::<&Mesh3d>(e).unwrap().0, mid);
        assert_eq!(MeshLod::new(1.0, &[full, mid]).meshes(), [full, mid]);
    }
}
