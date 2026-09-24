//! Export [`MeshData`] as Wavefront OBJ text or a self-contained glTF 2.0 document.

use super::{has_colors, has_normals, has_uvs};
use base64::Engine as _;
use serde_json::{Value, json};
use space3d_core::MeshData;
use std::fmt::Write as _;

/// Wavefront OBJ text. UVs are written with OBJ's bottom-left origin (`v' = 1 - v`) and
/// vertex colors, when present, as the common `v x y z r g b` extension.
pub fn to_obj(mesh: &MeshData) -> String {
    let (hn, hu, hc) = (has_normals(mesh), has_uvs(mesh), has_colors(mesh));
    let mut s = String::with_capacity(mesh.positions.len() * 96 + mesh.indices.len() * 12);
    s.push_str("# exported by space3d-assets\no mesh\n");
    for (i, p) in mesh.positions.iter().enumerate() {
        if hc {
            let c = mesh.colors[i];
            let _ = writeln!(s, "v {} {} {} {} {} {}", p[0], p[1], p[2], c[0], c[1], c[2]);
        } else {
            let _ = writeln!(s, "v {} {} {}", p[0], p[1], p[2]);
        }
    }
    if hu {
        for uv in &mesh.uvs {
            let _ = writeln!(s, "vt {} {}", uv[0], 1.0 - uv[1]);
        }
    }
    if hn {
        for n in &mesh.normals {
            let _ = writeln!(s, "vn {} {} {}", n[0], n[1], n[2]);
        }
    }
    for tri in mesh.indices.as_chunks::<3>().0 {
        s.push('f');
        for &i in tri {
            let i = i + 1;
            let _ = match (hu, hn) {
                (true, true) => write!(s, " {i}/{i}/{i}"),
                (true, false) => write!(s, " {i}/{i}"),
                (false, true) => write!(s, " {i}//{i}"),
                (false, false) => write!(s, " {i}"),
            };
        }
        s.push('\n');
    }
    s
}

const FLOAT: u32 = 5126;
const UNSIGNED_INT: u32 = 5125;
const ARRAY_BUFFER: u32 = 34962;
const ELEMENT_ARRAY_BUFFER: u32 = 34963;

struct Builder {
    buffer: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Builder {
    fn push(&mut self, bytes: Vec<u8>, target: u32, mut accessor: Value) -> usize {
        while !self.buffer.len().is_multiple_of(4) {
            self.buffer.push(0);
        }
        let offset = self.buffer.len();
        let len = bytes.len();
        self.buffer.extend(bytes);
        self.views.push(json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": len,
            "target": target,
        }));
        accessor["bufferView"] = json!(self.views.len() - 1);
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }

    fn floats<const N: usize>(&mut self, data: &[[f32; N]], ty: &str, min_max: bool) -> usize {
        let bytes = data.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let mut acc = json!({ "componentType": FLOAT, "count": data.len(), "type": ty });
        if min_max {
            let min: Vec<f32> = (0..N)
                .map(|k| data.iter().map(|v| v[k]).fold(f32::INFINITY, f32::min))
                .collect();
            let max: Vec<f32> = (0..N)
                .map(|k| data.iter().map(|v| v[k]).fold(f32::NEG_INFINITY, f32::max))
                .collect();
            acc["min"] = json!(min);
            acc["max"] = json!(max);
        }
        self.push(bytes, ARRAY_BUFFER, acc)
    }
}

/// A self-contained glTF 2.0 JSON document (single mesh/node, buffer embedded as a base64
/// data URI) that the [`crate::GltfLoader`] and other glTF tools can read.
pub fn to_gltf(mesh: &MeshData) -> String {
    let asset = json!({ "version": "2.0", "generator": "space3d-assets" });
    if mesh.positions.is_empty() || mesh.indices.len() < 3 {
        let doc = json!({ "asset": asset, "scene": 0, "scenes": [{ "nodes": [] }] });
        return serde_json::to_string_pretty(&doc).unwrap_or_default();
    }
    let mut b = Builder {
        buffer: Vec::new(),
        views: Vec::new(),
        accessors: Vec::new(),
    };
    let mut attributes = serde_json::Map::new();
    attributes.insert("POSITION".into(), json!(b.floats(&mesh.positions, "VEC3", true)));
    if has_normals(mesh) {
        attributes.insert("NORMAL".into(), json!(b.floats(&mesh.normals, "VEC3", false)));
    }
    if has_uvs(mesh) {
        attributes.insert("TEXCOORD_0".into(), json!(b.floats(&mesh.uvs, "VEC2", false)));
    }
    if has_colors(mesh) {
        attributes.insert("COLOR_0".into(), json!(b.floats(&mesh.colors, "VEC4", false)));
    }
    let indices = mesh.indices.len() / 3 * 3;
    let index_bytes = mesh.indices[..indices].iter().flat_map(|i| i.to_le_bytes()).collect();
    let index_acc = b.push(
        index_bytes,
        ELEMENT_ARRAY_BUFFER,
        json!({ "componentType": UNSIGNED_INT, "count": indices, "type": "SCALAR" }),
    );
    let uri = format!(
        "data:application/octet-stream;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&b.buffer)
    );
    let doc = json!({
        "asset": asset,
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0, "name": "mesh" }],
        "meshes": [{
            "name": "mesh",
            "primitives": [{ "attributes": attributes, "indices": index_acc, "mode": 4 }],
        }],
        "buffers": [{ "byteLength": b.buffer.len(), "uri": uri }],
        "bufferViews": b.views,
        "accessors": b.accessors,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::LoadContext;
    use crate::loaders::{load_gltf, load_obj};

    fn sample() -> MeshData {
        let mut m = MeshData::sphere(1.5, 8, 5);
        m.colors = (0..m.vertex_count())
            .map(|i| [i as f32 / 100.0, 0.5, 0.25, 1.0])
            .collect();
        m
    }

    #[test]
    fn gltf_roundtrip_is_exact() {
        let m = sample();
        let text = to_gltf(&m);
        let model = load_gltf(text.as_bytes(), &LoadContext::new("m.gltf"), true).unwrap();
        assert_eq!(model.meshes.len(), 1);
        assert_eq!(model.meshes[0].primitives[0].mesh, m);
        assert_eq!(model.roots, vec![0]);
        let empty = to_gltf(&MeshData::default());
        assert!(
            load_gltf(empty.as_bytes(), &LoadContext::new("e.gltf"), true)
                .unwrap()
                .meshes
                .is_empty()
        );
    }

    #[test]
    fn obj_roundtrip() {
        let m = sample();
        let text = to_obj(&m);
        assert!(text.contains("\nf 1/1/1 2/2/2"));
        let model = load_obj(text.as_bytes(), &LoadContext::new("m.obj"), false).unwrap();
        let back = &model.meshes[0].primitives[0].mesh;
        // tobj re-indexes vertices in order of first use, so compare triangle corners.
        assert_eq!(back.triangle_count(), m.triangle_count());
        assert_eq!(back.vertex_count(), m.vertex_count());
        for (&i, &j) in back.indices.iter().zip(&m.indices) {
            let (i, j) = (i as usize, j as usize);
            assert_eq!(back.positions[i], m.positions[j]);
            assert_eq!(back.normals[i], m.normals[j]);
            assert!((back.uvs[i][0] - m.uvs[j][0]).abs() < 1e-6 && (back.uvs[i][1] - m.uvs[j][1]).abs() < 1e-6);
            assert_eq!(&back.colors[i][..3], &m.colors[j][..3]);
        }
        let mut bare = MeshData::cube(1.0);
        bare.uvs.clear();
        bare.normals.clear();
        assert!(to_obj(&bare).contains("\nf 1 2 3\n"));
    }
}
