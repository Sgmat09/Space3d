//! glTF materials "just light up" (graphics spec §5.2 PM2): every material slot, `texCoord`
//! sets, samplers, `KHR_texture_transform`, `KHR_materials_emissive_strength`,
//! `KHR_materials_unlit`, `TANGENT` / `TEXCOORD_1`, ORM packing, color spaces, the render
//! bridge (`material_from_data`, `apply_material_maps`, generated MikkTSpace tangents) — and
//! the in-tree test ship (`crates/space3d-stories/assets/ship.glb`), a small hull with baked
//! base color, normal, ORM and emissive maps, which the `Engine/Materials/glTF Ship` story
//! renders. Its bytes are regenerated here and must match the checked-in file:
//! `cargo test -p space3d-assets --test gltf_materials -- --ignored regenerate_ship_glb`.

use image::{ImageEncoder, codecs::png::PngEncoder};
use serde_json::{Value, json};
use space3d_assets::loaders::load_gltf;
use space3d_assets::*;
use space3d_core::{ImageData, MeshData};

/// A minimal glTF 2.0 writer: one binary buffer (views 4-byte aligned), PNG images in it.
#[derive(Default)]
struct Builder {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
    images: Vec<Value>,
}

impl Builder {
    fn view(&mut self, bytes: &[u8]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        self.views
            .push(json!({ "buffer": 0, "byteOffset": self.bin.len(), "byteLength": bytes.len() }));
        self.bin.extend_from_slice(bytes);
        self.views.len() - 1
    }

    fn floats<const N: usize>(&mut self, data: &[[f32; N]], ty: &str, bounds: bool) -> usize {
        let bytes: Vec<u8> = data.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let view = self.view(&bytes);
        let mut a = json!({ "bufferView": view, "componentType": 5126, "count": data.len(), "type": ty });
        if bounds {
            let min: Vec<f32> = (0..N)
                .map(|i| data.iter().map(|v| v[i]).fold(f32::MAX, f32::min))
                .collect();
            let max: Vec<f32> = (0..N)
                .map(|i| data.iter().map(|v| v[i]).fold(f32::MIN, f32::max))
                .collect();
            a["min"] = json!(min);
            a["max"] = json!(max);
        }
        self.accessors.push(a);
        self.accessors.len() - 1
    }

    fn indices(&mut self, data: &[u32]) -> usize {
        let bytes: Vec<u8> = data.iter().flat_map(|i| i.to_le_bytes()).collect();
        let view = self.view(&bytes);
        self.accessors
            .push(json!({ "bufferView": view, "componentType": 5125, "count": data.len(), "type": "SCALAR" }));
        self.accessors.len() - 1
    }

    fn png(&mut self, img: &ImageData) -> usize {
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&img.pixels, img.width, img.height, image::ExtendedColorType::Rgba8)
            .unwrap();
        let view = self.view(&png);
        self.images.push(json!({ "bufferView": view, "mimeType": "image/png" }));
        self.images.len() - 1
    }

    /// A primitive's attributes (and indices) from a mesh plus optional tangents / UV1.
    fn primitive(&mut self, mesh: &MeshData, tangents: &[[f32; 4]], uvs1: &[[f32; 2]], material: usize) -> Value {
        let mut attributes = json!({
            "POSITION": self.floats(&mesh.positions, "VEC3", true),
            "NORMAL": self.floats(&mesh.normals, "VEC3", false),
            "TEXCOORD_0": self.floats(&mesh.uvs, "VEC2", false),
        });
        if !tangents.is_empty() {
            attributes["TANGENT"] = json!(self.floats(tangents, "VEC4", false));
        }
        if !uvs1.is_empty() {
            attributes["TEXCOORD_1"] = json!(self.floats(uvs1, "VEC2", false));
        }
        json!({ "attributes": attributes, "indices": self.indices(&mesh.indices), "material": material })
    }

    /// The document as a binary glTF (`.glb`): JSON chunk padded with spaces, BIN chunk with
    /// zeros.
    fn glb(mut self, mut doc: Value) -> Vec<u8> {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        doc["asset"] = json!({ "version": "2.0", "generator": "space3d-assets tests" });
        doc["buffers"] = json!([{ "byteLength": self.bin.len() }]);
        doc["bufferViews"] = json!(self.views);
        doc["accessors"] = json!(self.accessors);
        doc["images"] = json!(self.images);
        let mut text = serde_json::to_vec(&doc).unwrap();
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let total = 12 + 8 + text.len() + 8 + self.bin.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&text);
        out.extend_from_slice(&(self.bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&self.bin);
        out
    }
}

fn load(bytes: &[u8]) -> ModelData {
    load_gltf(bytes, &LoadContext::new("test.glb"), true).expect("valid glTF")
}

/// A document using every material feature the loader reads: two materials over five images
/// (the MR and occlusion slots share image 2: ORM packed), samplers, `texCoord` 1, texture
/// transforms, emissive strength, unlit, and a primitive with `TANGENT` and `TEXCOORD_1`.
fn all_slots() -> Vec<u8> {
    let mut b = Builder::default();
    let images: Vec<usize> = (0..5u8)
        .map(|i| b.png(&ImageData::filled(2, 2, [i * 40, 100, 200, 255])))
        .collect();
    let quad = MeshData::plane(1.0);
    let tangents = vec![[1.0, 0.0, 0.0, -1.0]; 4];
    let uvs1 = vec![[0.25, 0.75]; 4];
    let p0 = b.primitive(&quad, &tangents, &uvs1, 0);
    let p1 = b.primitive(&quad, &[], &[], 1);
    let transform = json!({ "KHR_texture_transform": { "offset": [0.5, 0.25], "rotation": 0.5, "scale": [2.0, 3.0] } });
    let doc = json!({
        "extensionsUsed": ["KHR_texture_transform", "KHR_materials_emissive_strength", "KHR_materials_unlit"],
        "samplers": [
            { "magFilter": 9728, "minFilter": 9984, "wrapS": 33071, "wrapT": 33071 },
            { "wrapS": 33648 }
        ],
        "textures": [
            { "source": images[0], "sampler": 0 },
            { "source": images[1] },
            { "source": images[2], "sampler": 1 },
            { "source": images[3] },
            { "source": images[4] },
            { "source": images[2] }
        ],
        "materials": [
            {
                "name": "hull",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [0.5, 0.6, 0.7, 1.0],
                    "baseColorTexture": { "index": 0, "extensions": transform },
                    "metallicFactor": 0.8,
                    "roughnessFactor": 0.4,
                    "metallicRoughnessTexture": { "index": 2 }
                },
                "normalTexture": { "index": 1, "scale": 0.7, "texCoord": 1, "extensions": transform },
                "occlusionTexture": { "index": 5, "strength": 0.6 },
                "emissiveTexture": { "index": 3, "texCoord": 1 },
                "emissiveFactor": [1.0, 0.5, 0.25],
                "extensions": { "KHR_materials_emissive_strength": { "emissiveStrength": 4.0 } },
                "alphaMode": "MASK",
                "alphaCutoff": 0.3,
                "doubleSided": true
            },
            {
                "name": "glow",
                "pbrMetallicRoughness": { "baseColorTexture": { "index": 4 } },
                "occlusionTexture": { "index": 3 },
                "alphaMode": "BLEND",
                "extensions": { "KHR_materials_unlit": {} }
            }
        ],
        "meshes": [{ "name": "quad", "primitives": [p0, p1] }],
        "nodes": [{ "mesh": 0 }],
        "scenes": [{ "nodes": [0] }],
        "scene": 0
    });
    b.glb(doc)
}

#[test]
fn gltf_reads_all_material_slots() {
    let model = load(&all_slots());
    assert_eq!(model.images.len(), 5);
    let hull = &model.materials[0];
    assert_eq!(hull.base_color_texture, Some(0));
    assert_eq!(hull.normal_texture, Some(1));
    assert_eq!(hull.metallic_roughness_texture, Some(2));
    assert_eq!(hull.occlusion_texture, Some(2), "texture 5 shows image 2");
    assert_eq!(hull.emissive_texture, Some(3));
    assert_eq!((hull.normal_scale, hull.occlusion_strength), (0.7, 0.6));
    assert_eq!((hull.metallic, hull.roughness), (0.8, 0.4));
    assert_eq!(hull.emissive, [1.0, 0.5, 0.25]);
    assert_eq!(hull.emissive_strength, 4.0);
    assert_eq!(hull.alpha_mode, AlphaMode::Mask(0.3));
    assert!(hull.double_sided && !hull.unlit);
    let t = &hull.texture_info;
    assert_eq!(
        (t.normal.tex_coord, t.emissive.tex_coord, t.base_color.tex_coord),
        (1, 1, 0)
    );
    let transform = t.base_color.transform.expect("KHR_texture_transform");
    assert_eq!(
        (transform.offset, transform.rotation, transform.scale),
        ([0.5, 0.25], 0.5, [2.0, 3.0])
    );
    assert_eq!(t.normal.transform, Some(transform), "raw extension of normalTexture");
    assert_eq!(t.metallic_roughness.transform, None);
    assert_eq!(
        t.base_color.sampler,
        SamplerData {
            wrap_s: WrapMode::ClampToEdge,
            wrap_t: WrapMode::ClampToEdge,
            nearest_mag: true,
            nearest_min: true,
        }
    );
    assert_eq!(t.metallic_roughness.sampler.wrap_s, WrapMode::MirroredRepeat);
    let glow = &model.materials[1];
    assert!(glow.unlit);
    assert_eq!(glow.alpha_mode, AlphaMode::Blend);
    assert_eq!((glow.base_color_texture, glow.occlusion_texture), (Some(4), Some(3)));
    assert_eq!(glow.emissive_strength, 1.0);
    // TANGENT and TEXCOORD_1 of the first primitive; the second has none.
    let mesh = &model.meshes[0];
    let extras = mesh.primitive_extras(0).expect("extras");
    assert_eq!(extras.tangents, vec![[1.0, 0.0, 0.0, -1.0]; 4]);
    assert_eq!(extras.uvs1, vec![[0.25, 0.75]; 4]);
    assert_eq!(mesh.primitive_extras(1), None);
}

/// A model without TANGENT / TEXCOORD_1 has no extras at all (and loads as before).
#[test]
fn gltf_without_extras() {
    let mut b = Builder::default();
    let p = b.primitive(&MeshData::cube(1.0), &[], &[], 0);
    let doc = json!({
        "materials": [{}],
        "meshes": [{ "primitives": [p] }],
        "nodes": [{ "mesh": 0 }],
        "scenes": [{ "nodes": [0] }]
    });
    let model = load(&b.glb(doc));
    assert!(model.meshes[0].extras.is_empty());
    let m = &model.materials[0];
    assert_eq!(
        (m.normal_scale, m.occlusion_strength, m.emissive_strength),
        (1.0, 1.0, 1.0)
    );
    assert_eq!(m.texture_info, MaterialTextureInfo::default());
}

#[cfg(feature = "render")]
mod bridge {
    use super::*;
    use glam::{Vec2, Vec3, Vec4};
    use space3d_assets::render::*;
    use space3d_render::{BlendMode, MapMask, Material, RenderAssets, TextureHandle};

    fn handles(n: usize) -> (RenderAssets, Vec<TextureHandle>) {
        let mut assets = RenderAssets::default();
        let h = (0..n).map(|_| assets.add_image(ImageData::new(1, 1))).collect();
        (assets, h)
    }

    /// The same image in the metallic-roughness and occlusion slots is one ORM texture: the
    /// material gets one handle for both (the renderer then samples it once).
    #[test]
    fn orm_packed_detection() {
        let model = load(&all_slots());
        let (_, h) = handles(5);
        let mut calls = Vec::new();
        let m = material_from_data(&model.materials[0], &mut |i, role| {
            calls.push((i, role));
            h.get(i).copied()
        });
        assert_eq!(m.metallic_roughness_texture, Some(h[2]));
        assert_eq!(m.occlusion_texture, m.metallic_roughness_texture, "ORM packed");
        assert_eq!(
            calls.iter().filter(|(i, _)| *i == 2).count(),
            1,
            "the packed image is requested once: {calls:?}"
        );
        // Different images: separate maps.
        let separate = MaterialData {
            metallic_roughness_texture: Some(1),
            occlusion_texture: Some(2),
            ..MaterialData::default()
        };
        let m = material_from_data(&separate, &mut |i, _| h.get(i).copied());
        assert_ne!(m.occlusion_texture, m.metallic_roughness_texture);
        // Through `upload_model`: one texture for the ORM image (linear), whatever glTF
        // texture objects point at it.
        let mut assets = RenderAssets::default();
        let r = upload_model(&model, &mut assets, &ModelUploadOptions::default());
        let hull = &r.parts[0].material;
        assert_eq!(hull.metallic_roughness_texture, hull.occlusion_texture);
    }

    /// Base color and emissive are uploaded sRGB, the data maps linear; an image used both
    /// ways is uploaded twice; the sampler comes from the glTF sampler of the slot.
    #[test]
    fn material_from_data_color_spaces() {
        let model = load(&all_slots());
        let (_, h) = handles(5);
        let mut roles = Vec::new();
        material_from_data(&model.materials[0], &mut |i, role| {
            roles.push((i, role.is_srgb()));
            h.get(i).copied()
        });
        assert_eq!(roles, [(0, true), (1, false), (2, false), (3, true)]);
        assert!(TextureRole::BaseColor.is_srgb() && TextureRole::Emissive.is_srgb());
        assert!(!TextureRole::Normal.is_srgb() && !TextureRole::MetallicRoughness.is_srgb());
        assert!(!TextureRole::Occlusion.is_srgb());
        // Image 3 is the hull's emissive (sRGB) and the glow's occlusion (linear): two uploads.
        let mut assets = RenderAssets::default();
        let r = upload_model(&model, &mut assets, &ModelUploadOptions::default());
        let hull = &r.parts[0].material;
        let glow = &r.parts[1].material;
        assert_ne!(hull.emissive_texture, glow.occlusion_texture);
        assert_eq!(r.textures.len(), 6, "images 0–4, image 3 twice");
        // The converted materials: blend modes, unlit, factors, maps on the second UV set,
        // the material's UV transform.
        assert_eq!(hull.effective_blend(), BlendMode::AlphaMask);
        assert_eq!(hull.alpha_cutoff, 0.3);
        assert_eq!(
            hull.emissive,
            Vec3::new(4.0, 2.0, 1.0),
            "× KHR_materials_emissive_strength"
        );
        assert_eq!(hull.uv1_maps, MapMask::NORMAL | MapMask::EMISSIVE);
        assert_eq!(hull.uv_transform.offset, Vec2::new(0.5, 0.25));
        assert_eq!(
            (hull.uv_transform.rotation, hull.uv_transform.scale),
            (0.5, Vec2::new(2.0, 3.0))
        );
        assert_eq!(hull.normal_scale, 0.7);
        assert!(glow.unlit && glow.effective_blend() == BlendMode::AlphaBlend);
    }

    /// `apply_material_maps` adds the maps to a material built by custom code (the game's
    /// `painted_material`): its base color, surface factors, blend and double-sidedness stay.
    #[test]
    fn apply_material_maps_keeps_custom_factors() {
        let model = load(&all_slots());
        let (_, h) = handles(5);
        let data = &model.materials[0];
        let custom = Material {
            double_sided: false,
            ..Material::color(Vec4::new(0.1, 0.2, 0.3, 1.0)).with_metallic_roughness(0.9, 0.35)
        };
        let mut m = custom.clone();
        apply_material_maps(&mut m, data, &mut |i, _| h.get(i).copied());
        assert_eq!((m.base_color, m.metallic, m.roughness), (custom.base_color, 0.9, 0.35));
        assert_eq!(
            (m.blend, m.double_sided, m.base_color_texture),
            (BlendMode::Opaque, false, None)
        );
        assert_eq!(m.normal_texture, Some(h[1]));
        assert_eq!(m.metallic_roughness_texture, m.occlusion_texture);
        assert_eq!((m.normal_scale, m.occlusion_strength), (0.7, 0.6));
        assert_eq!(m.emissive, Vec3::ZERO, "no glow where the custom material has none… ");
        assert_eq!(m.emissive_texture, Some(h[3]), "…though the map is set");
        assert_eq!(m.uv1_maps, MapMask::NORMAL | MapMask::EMISSIVE);
        assert_eq!(m.uv_transform.scale, Vec2::new(2.0, 3.0));
        // A glowing custom material gets the emissive strength.
        let mut glowing = custom.clone().with_emissive(Vec3::new(0.5, 0.5, 0.5));
        apply_material_maps(&mut glowing, data, &mut |i, _| h.get(i).copied());
        assert_eq!(glowing.emissive, Vec3::splat(2.0));
        // Data without maps changes nothing.
        let mut plain = custom.clone();
        apply_material_maps(&mut plain, &MaterialData::default(), &mut |_, _| None);
        assert_eq!(plain, custom);
    }
}

// ---- the test ship ----------------------------------------------------------------------------

/// Texture size of the ship's baked maps.
const MAP: u32 = 128;

/// UV regions of the atlas: hull panels, canopy (windows), engine glow.
const HULL: [f32; 4] = [0.0, 0.0, 0.5, 1.0];
const CANOPY: [f32; 4] = [0.5, 0.0, 0.75, 1.0];
const GLOW: [f32; 4] = [0.75, 0.0, 1.0, 1.0];

/// Panel height field of the hull region: grooves every 16 texels, rivets, a raised spine.
fn hull_height(x: i32, y: i32) -> f32 {
    let gx = x.rem_euclid(16);
    let gy = y.rem_euclid(24);
    let groove = |g: i32| -> f32 {
        if g == 0 {
            0.0
        } else if g == 1 || g == 15 {
            0.5
        } else {
            1.0
        }
    };
    let mut h: f32 = groove(gx).min(if gy == 0 { 0.0 } else { 1.0 });
    if (gx == 4 || gx == 12) && (gy == 4 || gy == 20) {
        h += 0.6;
    }
    h
}

/// The ship's maps: base color (sRGB), normal (linear), ORM (linear), emissive (sRGB).
fn ship_maps() -> [ImageData; 4] {
    let mut base = ImageData::new(MAP, MAP);
    let mut normal = ImageData::new(MAP, MAP);
    let mut orm = ImageData::new(MAP, MAP);
    let mut emissive = ImageData::new(MAP, MAP);
    let region = |x: u32| {
        let u = (x as f32 + 0.5) / MAP as f32;
        if u < HULL[2] {
            0
        } else if u < CANOPY[2] {
            1
        } else {
            2
        }
    };
    for y in 0..MAP {
        for x in 0..MAP {
            let (xi, yi) = (x as i32, y as i32);
            match region(x) {
                0 => {
                    let h = hull_height(xi, yi);
                    // Tangent-space normal: +X right, +Y up (rows go down).
                    let dx = (hull_height(xi + 1, yi) - hull_height(xi - 1, yi)) * 0.5;
                    let dy = -(hull_height(xi, yi + 1) - hull_height(xi, yi - 1)) * 0.5;
                    let n = glam::Vec3::new(-dx * 1.5, -dy * 1.5, 1.0).normalize();
                    let e = |c: f32| ((c * 0.5 + 0.5) * 255.0).round() as u8;
                    normal.set(x, y, [e(n.x), e(n.y), e(n.z), 255]);
                    let stripe = (40..48).contains(&y);
                    let panel = ((xi / 16 + yi / 24) % 3) as u8;
                    base.set(
                        x,
                        y,
                        if stripe {
                            [190, 70, 40, 255]
                        } else {
                            [120 + panel * 12, 128 + panel * 10, 140 + panel * 8, 255]
                        },
                    );
                    let occlusion = (150.0 + h.min(1.0) * 105.0) as u8;
                    let roughness = if h < 0.9 { 220 } else { 90 + panel * 25 };
                    orm.set(x, y, [occlusion, roughness, if stripe { 30 } else { 150 }, 255]);
                    emissive.set(x, y, [0, 0, 0, 255]);
                }
                1 => {
                    // Canopy: dark glass with lit window strips.
                    let window = y % 16 < 6 && (x % 8) < 6;
                    base.set(x, y, [20, 30, 40, 255]);
                    normal.set(x, y, [128, 128, 255, 255]);
                    orm.set(x, y, [255, 40, 0, 255]);
                    emissive.set(x, y, if window { [90, 220, 255, 255] } else { [0, 0, 0, 255] });
                }
                _ => {
                    // Engine glow: hot white core, orange rim.
                    let cx = (x as f32 + 0.5) / MAP as f32 - (GLOW[0] + GLOW[2]) * 0.5;
                    let cy = (y as f32 + 0.5) / MAP as f32 - 0.5;
                    let r = ((cx * 8.0).powi(2) + (cy * 2.0).powi(2)).sqrt().min(1.0);
                    let glow = 1.0 - r;
                    let c = |a: f32, b: f32| (a + (b - a) * glow).clamp(0.0, 255.0) as u8;
                    base.set(x, y, [30, 30, 34, 255]);
                    normal.set(x, y, [128, 128, 255, 255]);
                    orm.set(x, y, [255, 120, 180, 255]);
                    emissive.set(x, y, [c(90.0, 255.0), c(30.0, 230.0), c(0.0, 200.0), 255]);
                }
            }
        }
    }
    [base, normal, orm, emissive]
}

/// A box of the ship: `center`, `size`, the UV region of each face (in `MeshData::cube`'s face
/// order: +X, −X, +Y, −Y, +Z, −Z).
fn part(mesh: &mut MeshData, center: [f32; 3], size: [f32; 3], regions: [[f32; 4]; 6]) {
    let cube = MeshData::cube(1.0);
    let base = mesh.positions.len() as u32;
    for (i, p) in cube.positions.iter().enumerate() {
        let r = regions[i / 4];
        let [u, v] = cube.uvs[i];
        mesh.positions.push([0, 1, 2].map(|k| center[k] + p[k] * size[k]));
        mesh.normals.push(cube.normals[i]);
        mesh.uvs.push([r[0] + u * (r[2] - r[0]), r[1] + v * (r[3] - r[1])]);
    }
    mesh.indices.extend(cube.indices.iter().map(|i| i + base));
}

/// The ship's hull (+Z forward, flat on XZ like the game's top-down ships): a fuselage, a
/// canopy, wings and two engines whose back faces glow.
fn ship_mesh() -> MeshData {
    let mut m = MeshData::default();
    let hull = [HULL; 6];
    part(&mut m, [0.0, 0.0, 0.0], [0.8, 0.45, 3.0], hull);
    part(&mut m, [0.0, 0.32, 0.55], [0.5, 0.25, 1.0], [CANOPY; 6]);
    for side in [-1.0f32, 1.0] {
        part(&mut m, [side * 1.05, -0.05, -0.35], [1.4, 0.1, 1.3], hull);
        let mut engine = hull;
        engine[5] = GLOW;
        part(&mut m, [side * 0.5, 0.0, -1.55], [0.4, 0.4, 0.7], engine);
    }
    m
}

/// The test ship as a `.glb`: one ORM-packed material with every map, emissive strength 4, no
/// `TANGENT` (the render bridge generates MikkTSpace tangents, as for the game's ships).
fn ship_glb() -> Vec<u8> {
    let mut b = Builder::default();
    let [base, normal, orm, emissive] = ship_maps().map(|img| b.png(&img));
    let mesh = ship_mesh();
    let p = b.primitive(&mesh, &[], &[], 0);
    let doc = json!({
        "extensionsUsed": ["KHR_materials_emissive_strength"],
        "samplers": [{ "magFilter": 9729, "minFilter": 9987 }],
        "textures": [
            { "source": base, "sampler": 0 },
            { "source": normal, "sampler": 0 },
            { "source": orm, "sampler": 0 },
            { "source": emissive, "sampler": 0 }
        ],
        "materials": [{
            "name": "SC_Hull",
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicRoughnessTexture": { "index": 2 }
            },
            "normalTexture": { "index": 1 },
            "occlusionTexture": { "index": 2 },
            "emissiveTexture": { "index": 3 },
            "emissiveFactor": [1.0, 1.0, 1.0],
            "extensions": { "KHR_materials_emissive_strength": { "emissiveStrength": 4.0 } }
        }],
        "meshes": [{ "name": "ship", "primitives": [p] }],
        "nodes": [{ "name": "ship", "mesh": 0 }],
        "scenes": [{ "nodes": [0] }],
        "scene": 0
    });
    b.glb(doc)
}

fn ship_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../space3d-stories/assets/ship.glb")
}

/// The checked-in ship is what the generator writes (regenerate with the ignored test below).
#[test]
fn test_ship_matches_its_generator() {
    let bytes = std::fs::read(ship_path()).expect("crates/space3d-stories/assets/ship.glb");
    assert!(
        bytes == ship_glb(),
        "ship.glb is stale: run the ignored `regenerate_ship_glb`"
    );
    let model = load(&bytes);
    let m = &model.materials[0];
    assert_eq!(m.metallic_roughness_texture, m.occlusion_texture, "ORM packed");
    assert!(m.normal_texture.is_some() && m.emissive_texture.is_some() && m.base_color_texture.is_some());
    assert_eq!(m.emissive_strength, 4.0);
    assert!(model.meshes[0].extras.is_empty(), "no TANGENT: generated on upload");
    assert!(bytes.len() < 96 * 1024, "{} bytes", bytes.len());
}

#[test]
#[ignore = "writes crates/space3d-stories/assets/ship.glb"]
fn regenerate_ship_glb() {
    let path = ship_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, ship_glb()).unwrap();
}
