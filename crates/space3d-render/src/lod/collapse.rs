//! Edge-collapse simplification with quadric error metrics ([`simplify_edges`]).
//!
//! Vertex clustering ([`super::simplify`]) cannot make a thin part simpler: a fin a few cells
//! wide falls apart into scattered triangles before it loses many. Collapsing edges one at a
//! time, the cheapest first by the distance the surface moves (Garland and Heckbert's quadrics),
//! keeps a surface whole down to a small share of its triangles.

use crate::material::MeshExtras;
use glam::{DVec3, Vec2, Vec3};
use space3d_core::MeshData;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// What [`simplify_edges`] aims at and what it may not do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeCollapse {
    /// Stop at this many triangles (or sooner, when no collapse is left that is allowed).
    pub triangles: usize,
    /// No collapse may move the surface further than this, as a share of the mesh's largest
    /// side (0.01: a hundredth of it; the distance is the root mean square distance to the
    /// planes of the faces the collapsed vertex stood for). `f32::INFINITY`: no limit.
    pub max_error: f32,
    /// How much heavier than a face an open border or a knife edge weighs, so that the outline
    /// of an open sheet or of a thin fin stays where it is.
    pub border_weight: f32,
}

impl EdgeCollapse {
    /// Down to `triangles`, with no limit on the error and the default border weight (10).
    pub fn to(triangles: usize) -> Self {
        Self {
            triangles,
            max_error: f32::INFINITY,
            border_weight: 10.0,
        }
    }
    /// Down to `ratio` of the triangles of `data` (0.3: three in ten), at least one.
    pub fn to_ratio(data: &MeshData, ratio: f32) -> Self {
        Self::to(((data.triangle_count() as f32 * ratio.clamp(0.0, 1.0)) as usize).max(1))
    }
    /// With a limit on the error (see [`EdgeCollapse::max_error`]).
    pub fn with_max_error(mut self, max_error: f32) -> Self {
        self.max_error = max_error;
        self
    }
    /// With another weight for borders and knife edges.
    pub fn with_border_weight(mut self, weight: f32) -> Self {
        self.border_weight = weight;
        self
    }
}

/// Two faces over an edge whose normals are further apart than this (a dot product; -0.5: the
/// edge is sharper than 60 degrees) do not continue each other: the edge is a knife edge, or
/// the border of a sheet that has a reversed copy for its other side, and is held like a border.
const KNIFE_DOT: f64 = -0.5;
/// A collapse may not turn a face's normal further than this (a dot product; about 78 degrees).
const FLIP_DOT: f64 = 0.2;
/// A corner moved to another vertex takes the attributes of a corner there whose normal is at
/// least this close to its own (a dot product; 60 degrees), where it has no neighbour there.
const NORMAL_DOT: f32 = 0.5;
/// A little of an edge's length in a collapse's cost, so that among equally flat places the
/// short edges go first and the triangles stay well shaped.
const LENGTH_COST: f64 = 1e-4;

/// A plane's quadric: the sum of the weighted squared distances to a set of planes.
#[derive(Clone, Copy, Default)]
struct Quadric {
    /// The upper triangle of the 3 by 3 part: xx, xy, xz, yy, yz, zz.
    a: [f64; 6],
    b: [f64; 3],
    c: f64,
    weight: f64,
}

impl Quadric {
    fn plane(normal: DVec3, point: DVec3, weight: f64) -> Self {
        let d = -normal.dot(point);
        let (x, y, z) = (normal.x, normal.y, normal.z);
        Self {
            a: [x * x, x * y, x * z, y * y, y * z, z * z].map(|v| v * weight),
            b: [x * d, y * d, z * d].map(|v| v * weight),
            c: d * d * weight,
            weight,
        }
    }
    fn add(&mut self, o: &Quadric) {
        for i in 0..6 {
            self.a[i] += o.a[i];
        }
        for i in 0..3 {
            self.b[i] += o.b[i];
        }
        self.c += o.c;
        self.weight += o.weight;
    }
    /// The weighted sum of squared distances at `p` (never negative).
    fn at(&self, p: DVec3) -> f64 {
        let [xx, xy, xz, yy, yz, zz] = self.a;
        let v = p.x * (xx * p.x + 2.0 * xy * p.y + 2.0 * xz * p.z)
            + p.y * (yy * p.y + 2.0 * yz * p.z)
            + zz * p.z * p.z
            + 2.0 * (self.b[0] * p.x + self.b[1] * p.y + self.b[2] * p.z)
            + self.c;
        v.max(0.0)
    }
}

/// A collapse waiting in the queue: the place `from` moves onto the place `to`. Ordered by
/// cost, then by the two places, so that the order never depends on anything but the mesh.
#[derive(PartialEq)]
struct Candidate {
    cost: f64,
    from: u32,
    to: u32,
    stamps: (u32, u32),
}
impl Eq for Candidate {}
impl Ord for Candidate {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.cost
            .total_cmp(&o.cost)
            .then(self.from.cmp(&o.from))
            .then(self.to.cmp(&o.to))
            .then(self.stamps.cmp(&o.stamps))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

/// The mesh while it is made simpler. A *place* is a position in space; a *corner vertex* is a
/// vertex of the mesh given, with its attributes (several can stand at one place: the two
/// sides of a seam, of a hard edge, of a sheet).
struct Work<'a> {
    data: &'a MeshData,
    uvs1: &'a [[f32; 2]],
    has: Streams,
    /// The place of each vertex.
    place: Vec<u32>,
    /// A place's position.
    at: Vec<DVec3>,
    /// The triangles, by vertex; `[u32::MAX; 3]` once collapsed.
    tris: Vec<[u32; 3]>,
    /// The triangles at each place (collapsed ones are dropped as they are met).
    around: Vec<Vec<u32>>,
    quadric: Vec<Quadric>,
    /// Counts the changes around a place: a queued collapse made before the last is stale.
    stamp: Vec<u32>,
    alive: usize,
}

#[derive(Clone, Copy)]
struct Streams {
    normals: bool,
    uvs: bool,
    colors: bool,
    uvs1: bool,
}

const DEAD: [u32; 3] = [u32::MAX; 3];

impl Work<'_> {
    fn corners(&self, t: u32) -> [u32; 3] {
        self.tris[t as usize].map(|v| self.place[v as usize])
    }
    fn normal(&self, [a, b, c]: [DVec3; 3]) -> DVec3 {
        (b - a).cross(c - a)
    }
    /// The places joined to `p` by an edge, sorted.
    fn neighbours(&self, p: u32) -> Vec<u32> {
        let mut out: Vec<u32> = self.around[p as usize]
            .iter()
            .filter(|t| self.tris[**t as usize] != DEAD)
            .flat_map(|t| self.corners(*t))
            .filter(|q| *q != p)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
    /// Whether two vertices at one place lie at one place in the textures and the colours too
    /// (they differ in their normals at the most).
    fn same_paint(&self, a: u32, b: u32) -> bool {
        let (a, b) = (a as usize, b as usize);
        (!self.has.uvs || self.data.uvs[a] == self.data.uvs[b])
            && (!self.has.uvs1 || self.uvs1[a] == self.uvs1[b])
            && (!self.has.colors || self.data.colors[a] == self.data.colors[b])
    }
    fn cost(&self, from: u32, to: u32) -> f64 {
        let mut q = self.quadric[from as usize];
        q.add(&self.quadric[to as usize]);
        let (p, len2) = (
            self.at[to as usize],
            self.at[from as usize].distance_squared(self.at[to as usize]),
        );
        q.at(p) + LENGTH_COST * len2 * len2
    }
    /// The root mean square distance the collapse moves the surface.
    fn error(&self, from: u32, to: u32) -> f64 {
        let mut q = self.quadric[from as usize];
        q.add(&self.quadric[to as usize]);
        if q.weight > 0.0 {
            (q.at(self.at[to as usize]) / q.weight).sqrt()
        } else {
            0.0
        }
    }
    fn push(&self, heap: &mut BinaryHeap<Reverse<Candidate>>, from: u32, to: u32) {
        heap.push(Reverse(Candidate {
            cost: self.cost(from, to),
            from,
            to,
            stamps: (self.stamp[from as usize], self.stamp[to as usize]),
        }));
    }

    /// Whether the place `from` may move onto `to`, and if so the vertex at `to` each vertex at
    /// `from` becomes in the triangles that stay (`None`: it may not).
    fn plan(&self, from: u32, to: u32) -> Option<Vec<(u32, u32)>> {
        let live = |t: &&u32| self.tris[**t as usize] != DEAD;
        // The triangles on the edge go; each gives a pair of vertices, one at each end.
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        let mut thirds: Vec<u32> = Vec::new();
        for t in self.around[from as usize].iter().filter(live) {
            let (tri, places) = (self.tris[*t as usize], self.corners(*t));
            if let Some(j) = places.iter().position(|p| *p == to) {
                let i = places.iter().position(|p| *p == from)?;
                pairs.push((tri[i], tri[j]));
                thirds.push(places[3 - i - j]);
            }
        }
        if pairs.is_empty() {
            return None;
        }
        // The two places may share no neighbour but the third corners of the triangles between
        // them: the surface would be pinched into a sheet of two triangles back to back.
        let near = self.neighbours(to);
        if self
            .neighbours(from)
            .iter()
            .any(|p| near.binary_search(p).is_ok() && !thirds.contains(p))
        {
            return None;
        }
        let rotated = |mut k: [u32; 3]| {
            let least = (0..3).min_by_key(|i| k[*i]).unwrap_or(0);
            k.rotate_left(least);
            k
        };
        let there: Vec<[u32; 3]> = self.around[to as usize]
            .iter()
            .filter(live)
            .map(|t| rotated(self.corners(*t)))
            .collect();
        let mut map: Vec<(u32, u32)> = Vec::new();
        for t in self.around[from as usize].iter().filter(live) {
            let (tri, places) = (self.tris[*t as usize], self.corners(*t));
            if places.contains(&to) {
                continue;
            }
            let i = places.iter().position(|p| *p == from)?;
            // The face must keep its side and some area.
            let old = places.map(|p| self.at[p as usize]);
            let mut new = old;
            new[i] = self.at[to as usize];
            let (n0, n1) = (self.normal(old), self.normal(new));
            if n1.length_squared() <= n0.length_squared() * 1e-12 || n0.dot(n1) < FLIP_DOT * n0.length() * n1.length() {
                return None;
            }
            // And no triangle may come to lie on one that is there already.
            let mut moved = places;
            moved[i] = to;
            if there.contains(&rotated(moved)) {
                return None;
            }
            // The vertex at `to` this corner takes.
            let v = tri[i];
            let w = match map.iter().find(|m| m.0 == v) {
                Some(m) => m.1,
                None => {
                    let w = self.partner(v, &pairs)?;
                    map.push((v, w));
                    w
                }
            };
            // The face may not turn over in the texture either.
            if self.has.uvs {
                let uv = |v: u32| Vec2::from(self.data.uvs[v as usize]);
                let area = |k: [u32; 3]| (uv(k[1]) - uv(k[0])).perp_dot(uv(k[2]) - uv(k[0]));
                let mut k = tri;
                k[i] = w;
                let (before, after) = (area(tri), area(k));
                if before * after < 0.0 {
                    return None;
                }
            }
        }
        // The last triangle of a piece is not taken: a mesh is made simpler, not made to vanish.
        if map.is_empty()
            && self.around[to as usize]
                .iter()
                .filter(live)
                .all(|t| self.corners(*t).contains(&from))
        {
            return None;
        }
        Some(map)
    }

    /// The vertex at the far end of the edge that the vertex `v` of the near end becomes: the
    /// one it shares a triangle of the edge with; else the one a vertex painted like it shares
    /// one with, whose normal is nearest its own and not far from it. `None`: `v` is the other
    /// side of a seam or of a hard edge from all of them, and the collapse would tear it off.
    fn partner(&self, v: u32, pairs: &[(u32, u32)]) -> Option<u32> {
        let mut own = pairs.iter().filter(|p| p.0 == v).map(|p| p.1);
        if let Some(first) = own.next()
            && own.all(|w| w == first)
        {
            return Some(first);
        }
        let normal = |v: u32| Vec3::from(self.data.normals[v as usize]);
        let mut best: Option<(f32, u32)> = None;
        for (near, far) in pairs.iter().filter(|p| self.same_paint(p.0, v)) {
            let closeness = if self.has.normals {
                normal(v).dot(normal(*far)).min(normal(v).dot(normal(*near)))
            } else {
                1.0
            };
            let better = match best {
                None => true,
                Some((c, w)) => closeness > c || (closeness == c && *far < w),
            };
            if closeness >= NORMAL_DOT && better {
                best = Some((closeness, *far));
            }
        }
        best.map(|b| b.1)
    }

    fn collapse(&mut self, from: u32, to: u32, map: &[(u32, u32)]) {
        let moved = std::mem::take(&mut self.around[from as usize]);
        for t in moved {
            if self.tris[t as usize] == DEAD {
                continue;
            }
            if self.corners(t).contains(&to) {
                self.tris[t as usize] = DEAD;
                self.alive -= 1;
                continue;
            }
            for v in &mut self.tris[t as usize] {
                if self.place[*v as usize] == from
                    && let Some(m) = map.iter().find(|m| m.0 == *v)
                {
                    *v = m.1;
                }
            }
            self.around[to as usize].push(t);
        }
        let tris = &self.tris;
        self.around[to as usize].retain(|t| tris[*t as usize] != DEAD);
        let q = self.quadric[from as usize];
        self.quadric[to as usize].add(&q);
    }
}

/// A simpler copy of a mesh by edge collapse: again and again the edge whose loss moves the
/// surface least (by the quadric error metric: the summed squared distance to the planes of
/// the faces a vertex has stood for) is taken out, one of its ends moving onto the other, until
/// [`EdgeCollapse::triangles`] are left or no collapse within [`EdgeCollapse::max_error`] is.
/// Where vertex clustering ([`super::simplify`]) tears a thin part into scattered triangles,
/// this keeps a fin or a plate whole at a third of its triangles and less.
///
/// - **No new vertices.** An end of an edge moves onto the other end, so every vertex of the
///   result is a vertex of the mesh given, with its own normal, UVs, colour, tangent and second
///   UVs: no attribute is made up or blended, and a triangle is still textured from one island.
/// - **Seams and hard edges stay.** Vertices at one position with different attributes (the
///   two sides of a texture seam, of a hard edge, of a sheet with a reversed copy for its back)
///   are one place with several vertices. A place moves only where each of its vertices finds
///   its own side at the other end: along a seam, not off it, so both sides stay attached and
///   no triangle spans two islands; a corner where three sides meet does not move at all. A
///   mesh with a vertex for every corner of every face (flat shading) is joined by position,
///   and a moved corner takes the vertex there that is painted the same and faces nearest its
///   own way.
/// - **Borders and knife edges keep their outline.** An edge with one face, and an edge whose
///   faces fold back on each other more sharply than 60 degrees (the rim of a thin fin, or of a
///   sheet whose other side is a reversed copy of it), weighs [`EdgeCollapse::border_weight`]
///   times a face against moving sideways: it is made simpler along itself, not eaten into.
/// - **No face turns over**, in space (its normal may not turn more than about 78 degrees) or
///   in the texture, none collapses to a line, none comes to lie on another, and the surface is
///   never pinched: a closed mesh stays closed.
/// - **The same mesh every time**: ties are broken by the vertices' order, nothing by chance.
///
/// The result's vertices are those still used, in their old order. A stream (normals, UVs,
/// colours, tangents, second UVs) counts only when it has a value for every position, as in
/// [`super::simplify`]. Triangles that are a line or a point in the mesh given are dropped. A
/// mesh at or under the target comes back with its triangles as they are.
///
/// The cost is about `n log n` in the triangles: some tens of milliseconds for a hundred
/// thousand. Use it when a model is loaded, not in a frame.
pub fn simplify_edges(data: &MeshData, extras: &MeshExtras, settings: &EdgeCollapse) -> (MeshData, MeshExtras) {
    let n = data.positions.len();
    let has = Streams {
        normals: data.normals.len() == n,
        uvs: data.uvs.len() == n,
        colors: data.colors.len() == n,
        uvs1: extras.uvs1.len() == n,
    };
    // Places: the vertices by position (-0.0 is 0.0).
    let mut by_position: HashMap<[u32; 3], u32> = HashMap::with_capacity(n);
    let mut at: Vec<DVec3> = Vec::new();
    let place: Vec<u32> = data
        .positions
        .iter()
        .map(|p| {
            *by_position.entry(p.map(|c| (c + 0.0).to_bits())).or_insert_with(|| {
                at.push(Vec3::from(*p).as_dvec3());
                at.len() as u32 - 1
            })
        })
        .collect();
    let places = at.len();
    let mut work = Work {
        data,
        uvs1: &extras.uvs1,
        has,
        place,
        at,
        tris: Vec::with_capacity(data.indices.len() / 3),
        around: vec![Vec::new(); places],
        quadric: vec![Quadric::default(); places],
        stamp: vec![0; places],
        alive: 0,
    };
    for t in data.indices.as_chunks::<3>().0 {
        if t.iter().any(|i| *i as usize >= n) {
            continue;
        }
        let p = t.map(|v| work.place[v as usize]);
        if p[0] == p[1] || p[1] == p[2] || p[0] == p[2] {
            continue;
        }
        let id = work.tris.len() as u32;
        work.tris.push(*t);
        for q in p {
            work.around[q as usize].push(id);
        }
    }
    work.alive = work.tris.len();

    if work.alive > settings.triangles {
        // Each face's plane, weighted by its area, at its three places.
        let mut edges: HashMap<(u32, u32), Vec<u32>> = HashMap::with_capacity(work.tris.len() * 2);
        for t in 0..work.tris.len() as u32 {
            let p = work.corners(t);
            let normal = work.normal(p.map(|q| work.at[q as usize]));
            let area = normal.length() * 0.5;
            if area > 0.0 {
                let q = Quadric::plane(normal / (area * 2.0), work.at[p[0] as usize], area);
                for c in p {
                    work.quadric[c as usize].add(&q);
                }
            }
            for (a, b) in [(p[0], p[1]), (p[1], p[2]), (p[2], p[0])] {
                edges.entry((a.min(b), a.max(b))).or_default().push(t);
            }
        }
        // Borders and knife edges: for a face that no other continues over an edge, a plane
        // through the edge and square to the face.
        let mut keys: Vec<(u32, u32)> = edges.keys().copied().collect();
        keys.sort_unstable();
        let units: Vec<DVec3> = (0..work.tris.len() as u32)
            .map(|t| {
                work.normal(work.corners(t).map(|q| work.at[q as usize]))
                    .normalize_or_zero()
            })
            .collect();
        let unit = |t: u32| units[t as usize];
        for key in &keys {
            let on = &edges[key];
            let (a, b) = (work.at[key.0 as usize], work.at[key.1 as usize]);
            let (along, len2) = ((b - a).normalize_or_zero(), a.distance_squared(b));
            for t in on {
                let normal = unit(*t);
                if on.iter().any(|o| o != t && unit(*o).dot(normal) > KNIFE_DOT) {
                    continue;
                }
                let side = along.cross(normal).normalize_or_zero();
                if side != DVec3::ZERO {
                    let q = Quadric::plane(side, a, len2 * f64::from(settings.border_weight.max(0.0)));
                    work.quadric[key.0 as usize].add(&q);
                    work.quadric[key.1 as usize].add(&q);
                }
            }
        }
        let (min, max) = data.aabb();
        let limit = if settings.max_error.is_finite() {
            f64::from(settings.max_error.max(0.0)) * f64::from((max - min).max_element().max(0.0))
        } else {
            f64::INFINITY
        };
        let mut heap = BinaryHeap::with_capacity(keys.len() * 2);
        for (a, b) in &keys {
            work.push(&mut heap, *a, *b);
            work.push(&mut heap, *b, *a);
        }
        drop(edges);
        while work.alive > settings.triangles {
            let Some(Reverse(c)) = heap.pop() else {
                break;
            };
            if c.stamps != (work.stamp[c.from as usize], work.stamp[c.to as usize]) || work.error(c.from, c.to) > limit
            {
                continue;
            }
            let Some(map) = work.plan(c.from, c.to) else {
                continue;
            };
            work.collapse(c.from, c.to, &map);
            // Everything around the place that stayed is to be looked at again.
            let ring = work.neighbours(c.to);
            work.stamp[c.to as usize] += 1;
            for p in &ring {
                work.stamp[*p as usize] += 1;
            }
            for p in &ring {
                for q in work.neighbours(*p) {
                    // (an edge between two of the ring is pushed from its lower end only)
                    if q != c.to && ring.binary_search(&q).is_ok() && q < *p {
                        continue;
                    }
                    work.push(&mut heap, *p, q);
                    work.push(&mut heap, q, *p);
                }
            }
        }
    }

    // The vertices still used, in their old order.
    let mut used = vec![false; n];
    for t in work.tris.iter().filter(|t| **t != DEAD) {
        for v in t {
            used[*v as usize] = true;
        }
    }
    let mut index = vec![u32::MAX; n];
    let mut count = 0u32;
    for (i, u) in used.iter().enumerate() {
        if *u {
            index[i] = count;
            count += 1;
        }
    }
    fn kept<T: Copy>(used: &[bool], from: &[T]) -> Vec<T> {
        if from.len() == used.len() {
            from.iter().zip(used).filter(|(_, u)| **u).map(|(v, _)| *v).collect()
        } else {
            Vec::new()
        }
    }
    let out = MeshData {
        positions: kept(&used, &data.positions),
        normals: kept(&used, &data.normals),
        uvs: kept(&used, &data.uvs),
        colors: kept(&used, &data.colors),
        indices: work
            .tris
            .iter()
            .filter(|t| **t != DEAD)
            .flat_map(|t| t.map(|v| index[v as usize]))
            .collect(),
    };
    let mut out_extras = MeshExtras::new();
    out_extras.tangents = kept(&used, &extras.tangents);
    out_extras.uvs1 = kept(&used, &extras.uvs1);
    (out, out_extras)
}

#[cfg(test)]
#[path = "collapse_tests.rs"]
mod tests;
