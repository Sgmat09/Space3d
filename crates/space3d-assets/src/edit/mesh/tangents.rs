use space3d_core::MeshData;
use std::collections::HashMap;

/// MikkTSpace tangents `[x, y, z, w]` (w = ±1 bitangent sign) for normal mapping: splits
/// vertices whose per-corner tangents disagree (mirrored UVs), so it returns the (possibly
/// re-indexed) mesh with the tangents. ([`super::compute_tangents`] is Lengyel's per-vertex
/// average, not MikkTSpace.)
///
/// The tangents follow glTF: `bitangent = cross(normal, tangent.xyz) × w` points toward
/// decreasing v (up in the image, UV origin at the top left as in glTF and Space3d) — the
/// frame baked normal maps (Blender, Substance, xNormal) expect. Meshes without triangles or
/// UVs come back unchanged with no tangents (renderers then derive the frame per pixel).
pub fn compute_mikktspace_tangents(mesh: &MeshData) -> (MeshData, Vec<[f32; 4]>) {
    let (mesh, tangents, _) = mikktspace_with_sources(mesh);
    (mesh, tangents)
}

/// [`compute_mikktspace_tangents`], plus the source vertex of every output vertex (to carry
/// other per-vertex streams, like a second UV set, through the split).
pub(crate) fn mikktspace_with_sources(mesh: &MeshData) -> (MeshData, Vec<[f32; 4]>, Vec<u32>) {
    let n = mesh.positions.len();
    let identity = || (0..n as u32).collect::<Vec<u32>>();
    let indexed = !mesh.indices.is_empty();
    let corners = if indexed { mesh.indices.len() } else { n } / 3 * 3;
    if corners == 0 || mesh.uvs.len() != n {
        return (mesh.clone(), Vec::new(), identity());
    }
    let mut geometry = Geometry {
        mesh,
        indexed,
        faces: corners / 3,
        corners: vec![[1.0, 0.0, 0.0, 1.0]; corners],
    };
    // (The error type has no variants: generation always succeeds.)
    let _ = bevy_mikktspace::generate_tangents(&mut geometry);
    let corner_tangents = geometry.corners;

    // One output vertex per (source vertex, tangent): the first tangent a vertex meets keeps
    // it, corners with another one get a copy.
    let mut out = mesh.clone();
    if !indexed {
        out.indices = (0..n as u32).collect();
    }
    out.indices.truncate(corners);
    let mut tangents = vec![[1.0, 0.0, 0.0, 1.0]; n];
    let mut sources = identity();
    let mut first: Vec<Option<[u32; 4]>> = vec![None; n];
    let mut copies: HashMap<(u32, [u32; 4]), u32> = HashMap::new();
    for (corner, t) in corner_tangents.into_iter().enumerate() {
        let v = out.indices[corner];
        let key = t.map(f32::to_bits);
        match first[v as usize] {
            None => {
                first[v as usize] = Some(key);
                tangents[v as usize] = t;
            }
            Some(k) if k == key => {}
            Some(_) => {
                let copy = *copies.entry((v, key)).or_insert_with(|| {
                    let i = v as usize;
                    out.positions.push(mesh.positions[i]);
                    if let Some(normal) = mesh.normals.get(i) {
                        out.normals.push(*normal);
                    }
                    out.uvs.push(mesh.uvs[i]);
                    if let Some(color) = mesh.colors.get(i) {
                        out.colors.push(*color);
                    }
                    tangents.push(t);
                    sources.push(v);
                    (out.positions.len() - 1) as u32
                });
                out.indices[corner] = copy;
            }
        }
    }
    if !indexed && copies.is_empty() {
        out.indices.clear();
    }
    (out, tangents, sources)
}

struct Geometry<'a> {
    mesh: &'a MeshData,
    indexed: bool,
    faces: usize,
    corners: Vec<[f32; 4]>,
}

impl Geometry<'_> {
    fn vertex(&self, face: usize, vert: usize) -> usize {
        let corner = face * 3 + vert;
        if self.indexed {
            self.mesh.indices[corner] as usize
        } else {
            corner
        }
    }
}

impl bevy_mikktspace::Geometry for Geometry<'_> {
    fn num_faces(&self) -> usize {
        self.faces
    }

    fn num_vertices_of_face(&self, _face: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        self.mesh.positions[self.vertex(face, vert)]
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        self.mesh
            .normals
            .get(self.vertex(face, vert))
            .copied()
            .unwrap_or([0.0, 1.0, 0.0])
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        // MikkTSpace's bitangent follows +v; glTF's points toward −v (UV origin at the top
        // left), so it sees v flipped — as the tools that bake glTF normal maps do.
        let [u, v] = self.mesh.uvs[self.vertex(face, vert)];
        [u, 1.0 - v]
    }

    fn set_tangent(&mut self, tangent_space: Option<bevy_mikktspace::TangentSpace>, face: usize, vert: usize) {
        if let Some(t) = tangent_space {
            self.corners[face * 3 + vert] = t.tangent_encoded();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    fn xyz(t: [f32; 4]) -> Vec3 {
        Vec3::new(t[0], t[1], t[2])
    }

    /// A quad facing +Z with glTF UVs: tangent +X, bitangent (cross(n, t) × w) toward the top
    /// of the image (+Y); a cube: every face right-handed, tangent along its +u axis.
    #[test]
    fn tangents_follow_gltf() {
        let quad = MeshData {
            positions: vec![[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0], [-1.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            colors: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let (out, t) = compute_mikktspace_tangents(&quad);
        assert_eq!(out, quad, "nothing to split");
        for tangent in &t {
            assert!(xyz(*tangent).abs_diff_eq(Vec3::X, 1e-5), "{tangent:?}");
            let b = Vec3::Z.cross(xyz(*tangent)) * tangent[3];
            assert!(b.abs_diff_eq(Vec3::Y, 1e-5), "bitangent {b}");
        }
        let cube = MeshData::cube(1.0);
        let (out, t) = compute_mikktspace_tangents(&cube);
        assert_eq!((out.positions.len(), t.len()), (24, 24));
        assert!(t.iter().all(|t| t[3] == 1.0), "{t:?}");
        // +Z face (the fifth): tangent +X.
        assert!(xyz(t[16]).abs_diff_eq(Vec3::X, 1e-5));
    }

    /// Two quads sharing an edge whose UVs mirror across it: the shared vertices get a
    /// tangent (and bitangent sign) per side, so they are split.
    #[test]
    fn mikktspace_splits_mirrored_uvs() {
        // Columns x = -1, 0, 1; u = |x| (mirrored at x = 0), v = 0 at the top.
        let mut mesh = MeshData::default();
        for (x, y) in [
            (-1.0f32, -1.0f32),
            (0.0, -1.0),
            (1.0, -1.0),
            (-1.0, 1.0),
            (0.0, 1.0),
            (1.0, 1.0),
        ] {
            mesh.positions.push([x, y, 0.0]);
            mesh.normals.push([0.0, 0.0, 1.0]);
            mesh.uvs.push([x.abs(), if y > 0.0 { 0.0 } else { 1.0 }]);
        }
        mesh.indices = vec![0, 1, 4, 0, 4, 3, 1, 2, 5, 1, 5, 4];
        let (out, t) = compute_mikktspace_tangents(&mesh);
        assert_eq!(out.positions.len(), 8, "the two shared vertices are split");
        assert_eq!(t.len(), 8);
        let tangent_at = |corner: usize| t[out.indices[corner] as usize];
        for corner in 0..6 {
            let tl = tangent_at(corner);
            assert!(
                xyz(tl).abs_diff_eq(Vec3::NEG_X, 1e-5) && tl[3] == -1.0,
                "left (mirrored) side: {tl:?}"
            );
        }
        for corner in 6..12 {
            let tr = tangent_at(corner);
            assert!(xyz(tr).abs_diff_eq(Vec3::X, 1e-5) && tr[3] == 1.0, "right side: {tr:?}");
        }
        // Both sides keep the bitangent toward the top of the image; the copies keep every
        // attribute of their source (the middle column).
        for (i, t) in t.iter().enumerate() {
            let b = Vec3::Z.cross(xyz(*t)) * t[3];
            assert!(b.abs_diff_eq(Vec3::Y, 1e-5), "vertex {i}: {b}");
        }
        for copy in 6..8 {
            assert_eq!(out.positions[copy][0], 0.0);
            assert_eq!(out.uvs[copy][0], 0.0);
            assert_eq!(out.normals[copy], [0.0, 0.0, 1.0]);
        }
        let (_, _, sources) = mikktspace_with_sources(&mesh);
        assert_eq!(&sources[..6], &[0, 1, 2, 3, 4, 5]);
        assert!(sources[6..].iter().all(|s| [1, 4].contains(s)));
        // Meshes without UVs are left alone.
        let bare = MeshData {
            uvs: Vec::new(),
            ..MeshData::cube(1.0)
        };
        assert_eq!(compute_mikktspace_tangents(&bare), (bare.clone(), Vec::new()));
    }
}
