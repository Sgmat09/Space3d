//! Loader tests: in-test encoded images, glTF/GLB/OBJ fixtures, loaded directly and
//! through the asynchronous asset server.

mod common;

use common::*;
use glam::Vec3;
use image::{ImageEncoder, codecs::png::PngEncoder, codecs::tga::TgaEncoder};
use space3d_assets::loaders::{load_gltf, load_obj};
use space3d_assets::*;
use space3d_core::{ImageData, MeshData};
use std::time::Duration;

fn encode_png_with_image_crate(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

#[test]
fn image_loader_decodes_png_and_tga() {
    let pixels: Vec<u8> = (0..3 * 2).flat_map(|i| [i as u8 * 40, 255 - i as u8, 7, 200]).collect();
    let png = encode_png_with_image_crate(3, 2, &pixels);
    let img = ImageLoader.load(&png, &LoadContext::new("a.png")).unwrap();
    assert_eq!((img.width, img.height), (3, 2));
    assert_eq!(img.pixels, pixels);

    // TGA has no magic bytes: the extension hint selects the decoder.
    let mut tga = Vec::new();
    TgaEncoder::new(&mut tga)
        .write_image(&pixels, 3, 2, image::ExtendedColorType::Rgba8)
        .unwrap();
    let img = ImageLoader.load(&tga, &LoadContext::new("dir/b.TGA")).unwrap();
    assert_eq!(img.pixels, pixels);

    let err = ImageLoader.load(b"garbage", &LoadContext::new("c.png")).unwrap_err();
    assert!(
        matches!(err, AssetError::Decode { ref path, .. } if path == "c.png"),
        "{err}"
    );
}

#[test]
fn gltf_triangle_fixture() {
    let bytes = std::fs::read(fixtures().join("triangle.gltf")).unwrap();
    let model = GltfLoader::default()
        .load(&bytes, &LoadContext::new("triangle.gltf"))
        .unwrap();
    assert_eq!(model.meshes.len(), 1);
    assert_eq!(model.meshes[0].name.as_deref(), Some("Triangle"));
    let prim = &model.meshes[0].primitives[0];
    assert_eq!(prim.mesh.vertex_count(), 3);
    assert_eq!(prim.mesh.indices, vec![0, 1, 2]);
    // No normals in the file → flat normals are generated.
    assert!(prim.mesh.normals.iter().all(|n| *n == [0.0, 0.0, 1.0]));
    assert_eq!(prim.mesh.uvs.len(), 3);
    assert_eq!(prim.material, Some(0));
    let mat = &model.materials[0];
    assert_eq!(mat.name.as_deref(), Some("Red"));
    assert_eq!(mat.base_color, [1.0, 0.0, 0.0, 1.0]);
    assert_eq!((mat.metallic, mat.roughness), (0.25, 0.75));
    assert_eq!(model.nodes[0].transform.translation, Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(model.roots, vec![0]);
    let merged = model.merged_mesh();
    assert_eq!(merged.aabb(), (Vec3::new(1.0, 2.0, 3.0), Vec3::new(2.0, 3.0, 3.0)));
}

#[test]
fn gltf_with_missing_external_buffer_fails_cleanly() {
    let bytes = std::fs::read(fixtures().join("textured_quad.gltf")).unwrap();
    let err = load_gltf(&bytes, &LoadContext::new("textured_quad.gltf"), true).unwrap_err();
    assert!(err.to_string().contains("textured%20quad.bin"), "{err}");
    let deps = GltfLoader::default()
        .dependencies(&bytes, &LoadContext::new("m/textured_quad.gltf"))
        .unwrap();
    assert_eq!(deps, vec!["textured quad.bin".to_string(), "checker.png".to_string()]);
    let geometry_only = GltfLoader { load_images: false }
        .dependencies(&bytes, &LoadContext::new("textured_quad.gltf"))
        .unwrap();
    assert_eq!(geometry_only, vec!["textured quad.bin".to_string()]);
}

#[test]
fn glb_binary_chunk() {
    // Build a GLB in-test from exported geometry.
    let mesh = MeshData::cube(1.0);
    let positions: Vec<u8> = mesh.positions.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
    let indices: Vec<u8> = mesh.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut bin = positions.clone();
    bin.extend(&indices);
    let json = serde_json::json!({
        "asset": {"version": "2.0"},
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "nodes": [{"mesh": 0}],
        "buffers": [{"byteLength": bin.len()}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": positions.len()},
            {"buffer": 0, "byteOffset": positions.len(), "byteLength": indices.len()}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 24, "type": "VEC3",
             "min": [-0.5, -0.5, -0.5], "max": [0.5, 0.5, 0.5]},
            {"bufferView": 1, "componentType": 5125, "count": 36, "type": "SCALAR"}
        ]
    });
    let mut json = serde_json::to_vec(&json).unwrap();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = Vec::new();
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json);
    glb.extend((bin.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(&bin);

    let model = GltfLoader::default().load(&glb, &LoadContext::new("cube.glb")).unwrap();
    let m = &model.meshes[0].primitives[0].mesh;
    // No NORMAL attribute → flat normals, which unwelds to one vertex per corner.
    assert_eq!(m.vertex_count(), 36);
    assert_eq!(m.indices, (0..36).collect::<Vec<u32>>());
    for (k, &i) in mesh.indices.iter().enumerate() {
        assert_eq!(m.positions[k], mesh.positions[i as usize]);
        assert_eq!(m.normals[k], mesh.normals[i as usize]);
    }
    // No scenes → roots are parentless nodes.
    assert_eq!(model.roots, vec![0]);
    let merged = MeshLoader(GltfLoader::default())
        .load(&glb, &LoadContext::new("cube.glb"))
        .unwrap();
    assert_eq!(merged.triangle_count(), 12);
}

#[test]
fn obj_fixture_with_in_test_texture() {
    let obj = std::fs::read(fixtures().join("cube.obj")).unwrap();
    let mtl = std::fs::read(fixtures().join("cube.mtl")).unwrap();
    let png = encode_png_with_image_crate(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
    let loader = ObjLoader::default();

    // Dependency discovery is iterative: first the MTL, then the textures it names.
    let ctx = LoadContext::new("models/cube.obj");
    assert_eq!(loader.dependencies(&obj, &ctx).unwrap(), vec!["cube.mtl".to_string()]);
    let ctx = ctx.with_dependency("cube.mtl", mtl);
    let deps = loader.dependencies(&obj, &ctx).unwrap();
    assert_eq!(deps, vec!["cube.mtl".to_string(), "/models/checker.png".to_string()]);
    let ctx = ctx.with_dependency("checker.png", png);

    let model = load_obj(&obj, &ctx, true).unwrap();
    assert_eq!(model.meshes.len(), 2, "one mesh per material group");
    assert_eq!(model.materials.len(), 2);
    let checker = &model.materials[0];
    assert_eq!(checker.name.as_deref(), Some("Checker"));
    assert_eq!(checker.base_color_texture, Some(0));
    assert_eq!(model.images[0].get(1, 0), [0, 255, 0, 255]);
    let blue = &model.materials[1];
    assert_eq!(blue.base_color, [0.0, 0.0, 1.0, 0.5]);
    assert_eq!(blue.alpha_mode, AlphaMode::Blend);
    let tris: usize = model.meshes.iter().map(|m| m.primitives[0].mesh.triangle_count()).sum();
    assert_eq!(tris, 12, "quads are triangulated");
    let front = &model.meshes[0].primitives[0].mesh;
    assert_eq!(front.uvs[0], [0.0, 1.0], "OBJ v is flipped to a top-left origin");
    assert_eq!(model.meshes[1].primitives[0].material, Some(1));
    let merged = model.merged_mesh();
    assert_eq!(merged.aabb(), (Vec3::splat(-0.5), Vec3::splat(0.5)));
}

#[test]
fn server_loads_gltf_with_external_dependencies() {
    let mut app = app_with_root(&fixtures());
    let server = app.resources.expect_mut::<AssetServer>();
    let model: Handle<ModelData> = server.load("textured_quad.gltf");
    let mesh: Handle<MeshData> = server.load("textured_quad.gltf");
    let tex: Handle<ImageData> = server.load("checker.png");
    assert_ne!(model.id().untyped(), mesh.id().untyped());
    wait_idle(&mut app);

    let server = app.resources.expect::<AssetServer>();
    for id in [model.id().untyped(), mesh.id().untyped(), tex.id().untyped()] {
        assert_eq!(server.load_state(id), LoadState::Loaded, "{:?}", server.path(id));
    }
    let deps: Vec<String> = server.dependencies(&model).iter().map(|p| p.to_string()).collect();
    assert!(deps.contains(&"textured quad.bin".to_string()) && deps.contains(&"checker.png".to_string()));

    let models = app.resources.expect::<Assets<ModelData>>();
    let m = models.get(&model).unwrap();
    assert_eq!(m.images.len(), 1);
    assert_eq!((m.images[0].width, m.images[0].height), (4, 4));
    assert_eq!(m.images[0].get(1, 0), [0, 0, 0, 255]);
    let mat = &m.materials[0];
    assert_eq!(mat.base_color_texture, Some(0));
    assert_eq!(mat.alpha_mode, AlphaMode::Mask(0.25));
    assert!(mat.double_sided);
    assert_eq!(m.nodes[1].parent, Some(0));
    assert_eq!(m.roots, vec![0]);

    let meshes = app.resources.expect::<Assets<MeshData>>();
    let baked = meshes.get(&mesh).unwrap();
    let (min, max) = baked.aabb();
    assert_eq!((min, max), (Vec3::new(-2.0, -2.0, 10.0), Vec3::new(2.0, 2.0, 10.0)));
    let images = app.resources.expect::<Assets<ImageData>>();
    assert_eq!(images.get(&tex).unwrap(), &m.images[0]);
}

#[test]
fn server_loads_obj_model_and_mesh() {
    let mut app = app_with_root(&fixtures());
    let server = app.resources.expect_mut::<AssetServer>();
    let model: Handle<ModelData> = server.load("cube.obj");
    let mesh: Handle<MeshData> = server.load("cube.obj");
    run_until(&mut app, Duration::from_secs(20), |app| {
        let s = app.resources.expect::<AssetServer>();
        s.is_loaded(&model) && s.is_loaded(&mesh)
    });
    let m = app.resources.expect::<Assets<ModelData>>().get(&model).unwrap().clone();
    assert_eq!(m.images.len(), 1);
    assert_eq!(m.images[0].width, 4);
    let server = app.resources.expect::<AssetServer>();
    assert_eq!(server.dependencies(&model).len(), 2);
    assert!(server.dependencies(&mesh).is_empty(), "mesh loader skips materials");
    let merged = app.resources.expect::<Assets<MeshData>>().get(&mesh).unwrap();
    assert_eq!(merged.triangle_count(), 12);
}
