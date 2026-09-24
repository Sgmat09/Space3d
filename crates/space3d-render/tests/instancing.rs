//! Instancing, culling, draw paths and upload accounting.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_render::*;

fn cube_grid(app: &mut App, n: i32, spacing: f32) -> MeshHandle {
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    let material = Material::srgb(0.7, 0.7, 0.9);
    for x in 0..n {
        for y in 0..n {
            for z in 0..n {
                let p = Vec3::new(x as f32, y as f32, z as f32) - Vec3::splat((n - 1) as f32 / 2.0);
                app.world.spawn((
                    Transform::from_xyz(p.x * spacing, p.y * spacing, p.z * spacing),
                    Mesh3d(mesh),
                    material.clone(),
                ));
            }
        }
    }
    mesh
}

#[test]
fn thousand_ecs_cubes_are_one_draw_call_on_every_path() {
    let Some(mut app) = headless_app(128, 128) else { return };
    cube_grid(&mut app, 10, 1.0);
    spawn_camera(&mut app, 25.0);
    spawn_sun(&mut app);
    let best = best_path(&app);
    for path in [
        DrawPath::CpuDirect,
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ] {
        if path > best {
            continue;
        }
        set_path(&mut app, path);
        render_to_image(&mut app).expect("capture");
        let s = stats(&app);
        assert_eq!(s.draw_path, path);
        assert_eq!(s.batches, 1, "{path:?}");
        assert_eq!(s.instances, 1000, "{path:?}");
        assert_eq!(s.draw_calls, 1, "{path:?}: 1000 cubes, same mesh+material → 1 draw");
        assert_eq!(s.triangles, 12_000, "{path:?}");
        assert_eq!(s.gpu_culling, path != DrawPath::CpuDirect);
    }
}

#[test]
fn cpu_culling_rejects_offscreen_instances() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    for i in 0..10 {
        let x = i as f32 * 0.6 - 3.0;
        app.world
            .spawn((Transform::from_xyz(x, 0.0, 0.0), Mesh3d(mesh), Material::default()));
        // Behind the camera and far to the side: never visible.
        app.world
            .spawn((Transform::from_xyz(x, 0.0, 50.0), Mesh3d(mesh), Material::default()));
        app.world.spawn((
            Transform::from_xyz(500.0 + x, 0.0, 0.0),
            Mesh3d(mesh),
            Material::default(),
        ));
    }
    spawn_camera(&mut app, 8.0);
    set_path(&mut app, DrawPath::CpuDirect);
    render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.instances, 30);
    assert_eq!(s.culled, 20);
    assert_eq!(s.instances_drawn, 10);
    assert_eq!(s.draw_calls, 1);

    app.resources.expect_mut::<RenderSettings>().frustum_culling = false;
    render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).culled, 0);
}

#[test]
fn gpu_culling_matches_visible_only_scene() {
    let Some(mut app) = headless_app(96, 96) else { return };
    let best = best_path(&app);
    if best == DrawPath::CpuDirect {
        eprintln!("skipping: GPU culling unsupported on this adapter");
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.5));
    let visible: Vec<_> = (0..20)
        .map(|i| {
            let p = Vec3::new((i % 5) as f32 - 2.0, (i / 5) as f32 - 1.5, 0.0);
            app.world.spawn((
                Transform::from_xyz(p.x, p.y, p.z),
                Mesh3d(mesh),
                Material::srgb(0.9, 0.5, 0.1),
            ))
        })
        .collect();
    spawn_camera(&mut app, 8.0);
    spawn_sun(&mut app);
    set_path(&mut app, best);
    let only_visible = render_to_image(&mut app).expect("capture");
    for i in 0..200 {
        let p = Vec3::new(1000.0 + i as f32, 0.0, -(i as f32));
        app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(mesh),
            Material::srgb(0.9, 0.5, 0.1),
        ));
    }
    let with_hidden = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instances, 220);
    assert!(
        diff_fraction(&only_visible, &with_hidden, 2) < 0.001,
        "GPU-culled instances must not change the image"
    );
    drop(visible);
}

/// 10k explicit instances: GPU-indirect path and CPU/direct path must render the same image.
#[test]
fn gpu_indirect_matches_cpu_direct_for_10k_instances() {
    let Some(mut app) = headless_app(160, 120) else { return };
    let best = best_path(&app);
    if best == DrawPath::CpuDirect {
        eprintln!("skipping: GPU culling unsupported on this adapter");
        return;
    }
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
    let mut buf = InstanceBuffer::with_capacity(10_000, BufferUsage::Static);
    for i in 0..10_000 {
        let (x, y) = ((i % 100) as f32, (i / 100) as f32);
        let p = Vec3::new(x * 0.5 - 25.0, y * 0.5 - 25.0, -((i * 7919) % 13) as f32);
        let rot = Quat::from_rotation_y(i as f32 * 0.37) * Quat::from_rotation_x(i as f32 * 0.11);
        buf.push(InstanceCompact::new(p, rot, 0.8 + (i % 5) as f32 * 0.1));
    }
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::srgb(0.3, 0.8, 0.5),
        instances,
    },));
    // Camera sees roughly half the grid so culling is exercised.
    app.world.spawn((
        Transform::from_xyz(10.0, 5.0, 20.0).looking_at(Vec3::new(10.0, 5.0, 0.0), Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
    ));
    spawn_sun(&mut app);

    set_path(&mut app, DrawPath::CpuDirect);
    let cpu = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).draw_calls, 1);
    let mut images = vec![];
    for path in [
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ] {
        if path > best {
            continue;
        }
        set_path(&mut app, path);
        let gpu = render_to_image(&mut app).expect("capture");
        let s = stats(&app);
        assert!(
            s.gpu_culling && s.indirect_draw_calls == 1 && s.compute_dispatches >= 1,
            "{path:?}: {s:?}"
        );
        let d = diff_fraction(&cpu, &gpu, 3);
        assert!(
            d < 0.002,
            "{path:?} differs from CPU path in {:.3}% of pixels",
            d * 100.0
        );
        images.push(gpu);
    }
    assert!(
        cpu.pixels.chunks(4).filter(|p| *p != CLEAR).count() > 1000,
        "instances visible"
    );
}

#[test]
fn dirty_range_uploads_only_changed_instances() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.1));
    let data: Vec<_> = (0..100_000)
        .map(|i| {
            InstanceCompact::new(
                Vec3::new((i % 1000) as f32 * 0.2, (i / 1000) as f32 * 0.2, 0.0),
                Quat::IDENTITY,
                1.0,
            )
        })
        .collect();
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    spawn_camera(&mut app, 50.0);
    render_to_image(&mut app).expect("capture");
    let first = stats(&app);
    assert_eq!(first.instance_bytes_uploaded, 100_000 * 32);

    let bufs = app.resources.expect_mut::<InstanceBuffers>();
    bufs.get_mut::<InstanceCompact>(h)
        .unwrap()
        .set(54_321, InstanceCompact::new(Vec3::ONE, Quat::IDENTITY, 1.0));
    render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.instance_bytes_uploaded, 32, "exactly one 32-byte instance uploaded");
    assert_eq!(s.buffer_reallocations, 0);

    // Many scattered edits coalesce but stay far below a full upload.
    let buf = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstanceCompact>(h)
        .unwrap();
    for i in (0..100_000).step_by(10_000) {
        buf.get_mut(i).scale = 2.0;
    }
    render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instance_bytes_uploaded, 10 * 32);
}

#[test]
fn static_buffer_and_static_scene_upload_nothing_after_first_frame() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.2));
    let buf = InstanceBuffer::from_vec(
        (0..5000)
            .map(|i| InstanceTransform::from_trs(Vec3::new(i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, Vec3::ONE))
            .collect(),
        BufferUsage::Static,
    );
    let h = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::default(),
        instances: h,
    },));
    cube_grid(&mut app, 3, 1.0);
    spawn_camera(&mut app, 10.0);
    spawn_sun(&mut app);
    for frame in 0..4 {
        app.step_ticks(1);
        let s = stats(&app);
        if frame == 0 {
            assert!(s.instance_bytes_uploaded >= 5000 * 64);
        } else {
            assert_eq!(s.instance_bytes_uploaded, 0, "frame {frame}: {s:?}");
            assert_eq!(
                s.bytes_uploaded, 0,
                "frame {frame}: unchanged scene uploads nothing: {s:?}"
            );
            assert_eq!(s.buffer_reallocations, 0, "frame {frame}");
            assert_eq!(
                s.cpu_reallocations, 0,
                "frame {frame}: no scratch growth in steady state"
            );
        }
    }
}

#[test]
fn moving_ecs_entities_upload_only_changed_matrices() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = cube_grid(&mut app, 10, 1.0);
    let mover = app
        .world
        .spawn((Transform::default(), Mesh3d(mesh), Material::srgb(0.7, 0.7, 0.9)));
    spawn_camera(&mut app, 30.0);
    app.step_ticks(2);
    app.world.get::<&mut Transform>(mover).unwrap().translation.x += 1.0;
    app.step_ticks(1);
    let s = stats(&app);
    assert_eq!(s.instance_bytes_uploaded, 64, "one moved entity = one matrix: {s:?}");
    assert_eq!(s.cpu_reallocations, 0);
}

#[test]
fn custom_packed_instances_with_color_and_layers() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let (mesh, tex) = {
        let a = app.resources.expect_mut::<RenderAssets>();
        let layers = [
            ImageData::filled(4, 4, [255, 0, 0, 255]),
            ImageData::filled(4, 4, [0, 255, 0, 255]),
        ];
        (
            a.add_mesh(MeshData::cube(1.0)),
            a.add_texture(TextureSource::from_layers(&layers).unwrap()),
        )
    };
    let mut buf = InstanceBuffer::new(BufferUsage::Stream);
    buf.push(InstancePacked::new(
        Vec3::new(-0.8, 0.0, 0.0),
        Quat::IDENTITY,
        1.0,
        Vec4::ONE,
        0,
    ));
    buf.push(InstancePacked::new(
        Vec3::new(0.8, 0.0, 0.0),
        Quat::IDENTITY,
        1.0,
        Vec4::ONE,
        1,
    ));
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE).with_texture(tex),
        instances,
    },));
    spawn_camera(&mut app, 4.0);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(20, 32), [255, 0, 0, 255], "layer 0 = red");
    assert_eq!(img.get(44, 32), [0, 255, 0, 255], "layer 1 = green");
    assert_eq!(
        stats(&app).instance_bytes_uploaded,
        64,
        "stream buffers upload fully every frame"
    );
}

#[test]
fn replacing_an_instance_buffer_in_a_reused_slot_uses_the_new_data() {
    let Some(mut app) = headless_app(32, 32) else { return };
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    let far = InstanceBuffer::from_vec(
        vec![InstanceCompact::new(Vec3::X * 100.0, Quat::IDENTITY, 1.0)],
        BufferUsage::Static,
    );
    let h1 = app.resources.expect_mut::<InstanceBuffers>().insert(far);
    let e = app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE),
        instances: h1,
    },));
    spawn_camera(&mut app, 3.0);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), CLEAR);

    let bufs = app.resources.expect_mut::<InstanceBuffers>();
    assert!(bufs.remove(h1));
    let near = InstanceBuffer::from_vec(
        vec![InstanceCompact::new(Vec3::ZERO, Quat::IDENTITY, 1.0)],
        BufferUsage::Static,
    );
    let h2 = bufs.insert(near);
    assert_eq!(h1.index(), h2.index(), "slot reused");
    app.world.get::<&mut InstancedMesh>(e).unwrap().instances = h2;
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(img.get(16, 16), [255, 255, 255, 255]);
}

fn gpu_paths_up_to(best: DrawPath) -> Vec<DrawPath> {
    [
        DrawPath::CpuDirect,
        DrawPath::GpuIndirect,
        DrawPath::GpuMultiDraw,
        DrawPath::GpuMultiDrawCount,
    ]
    .into_iter()
    .filter(|p| *p <= best)
    .collect()
}

/// Tiny deterministic PRNG for test scenes.
fn lcg(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*seed >> 8) as f32 / (1u32 << 24) as f32
}

/// Regression: GPU culling compacted visible instances (and draws) with atomics, so their order
/// changed from dispatch to dispatch and overlapping alpha-blended instances flickered. The
/// compaction is now order-preserving: every path renders identical bytes frame after frame,
/// and GPU paths draw in exactly the CPU path's order.
#[test]
fn blended_instances_render_deterministically_on_every_path() {
    let Some(mut app) = headless_app(64, 64) else { return };
    let best = best_path(&app);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(1.0));
    let mut seed = 12_345;
    let mut buf = InstanceBuffer::new(BufferUsage::Static);
    for i in 0..5000 {
        // 79 workgroups (scan lanes own blocks of several), a few culled instances between.
        let x = if i % 97 == 0 { 500.0 } else { 0.0 };
        let c = Vec4::new(lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0.35);
        buf.push(InstanceTransformColor::new(
            Mat4::from_translation(Vec3::new(x, 0.0, 0.0)),
            c,
        ));
    }
    let instances = app.resources.expect_mut::<InstanceBuffers>().insert(buf);
    let blend = Material {
        alpha_blend: true,
        double_sided: true,
        ..Material::unlit(Vec4::ONE)
    };
    app.world.spawn((InstancedMesh {
        mesh,
        material: blend.clone(),
        instances,
    },));
    // Plain ECS entities with a blended material take the same GPU path.
    let small = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.4));
    for i in 0..300 {
        let m = Material {
            base_color: Vec4::new(lcg(&mut seed), lcg(&mut seed), lcg(&mut seed), 0.4),
            ..blend.clone()
        };
        // Two materials alternate so ECS batches form several runs.
        let m = if i % 2 == 0 {
            m
        } else {
            Material {
                base_color: Vec4::new(0.2, 0.9, 0.3, 0.3),
                ..m
            }
        };
        app.world.spawn((
            Transform::from_xyz(lcg(&mut seed) * 0.5, lcg(&mut seed) * 0.5, 0.6)
                .with_rotation(Quat::from_rotation_z(lcg(&mut seed) * 6.0)),
            Mesh3d(if i % 3 == 0 { small } else { mesh }),
            m,
        ));
    }
    spawn_camera(&mut app, 4.0);

    let mut reference: Option<ImageData> = None;
    for path in gpu_paths_up_to(best) {
        set_path(&mut app, path);
        let first = render_to_image(&mut app).expect("capture");
        for frame in 1..8 {
            let img = render_to_image(&mut app).expect("capture");
            let differing = first
                .pixels
                .chunks(4)
                .zip(img.pixels.chunks(4))
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                differing, 0,
                "{path:?}: frame {frame} differs from frame 0 in {differing} px"
            );
        }
        match &reference {
            None => reference = Some(first),
            Some(cpu) => {
                let d = cpu
                    .pixels
                    .chunks(4)
                    .zip(first.pixels.chunks(4))
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(
                    d, 0,
                    "{path:?} draws in a different order than CpuDirect ({d} px differ)"
                );
            }
        }
    }
    let img = reference.expect("CpuDirect ran");
    assert!(img.pixels.chunks(4).filter(|p| *p != CLEAR).count() > 500);
}

/// Regression: `set_bounds(Some)` → edits → `set_bounds(None)` kept a stale automatic AABB that
/// no longer contained any instance, so the CPU path culled the whole buffer.
#[test]
fn clearing_user_bounds_recomputes_automatic_bounds() {
    let Some(mut app) = headless_app(64, 64) else { return };
    set_path(&mut app, DrawPath::CpuDirect);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.2));
    let data = (0..100)
        .map(|i| InstanceCompact::new(Vec3::new(i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, 1.0))
        .collect();
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE),
        instances: h,
    },));
    let cam = spawn_camera(&mut app, 10.0);
    render_to_image(&mut app).expect("capture");
    app.resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstanceCompact>(h)
        .unwrap()
        .set_bounds(Some(Aabb::new(Vec3::splat(-1e5), Vec3::splat(1e5))));
    // Move everything to x = 1000, 10 instances (< 50 %) per frame.
    for round in 0..10 {
        let buf = app
            .resources
            .expect_mut::<InstanceBuffers>()
            .get_mut::<InstanceCompact>(h)
            .unwrap();
        for i in round * 10..round * 10 + 10 {
            buf.set(
                i,
                InstanceCompact::new(Vec3::new(1000.0 + i as f32 * 0.01, 0.0, 0.0), Quat::IDENTITY, 1.0),
            );
        }
        render_to_image(&mut app).expect("capture");
    }
    app.resources
        .expect_mut::<InstanceBuffers>()
        .get_mut::<InstanceCompact>(h)
        .unwrap()
        .set_bounds(None);
    app.world.get::<&mut Transform>(cam).unwrap().translation.x = 1000.5;
    let img = render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!((s.instances_drawn, s.culled), (100, 0), "{s:?}");
    assert_eq!(img.get(32, 32), [255, 255, 255, 255]);
}

/// Thin quad in the XY plane along (1,1,0)/√2 (half-length `l`, half-width `w`), facing +Z.
fn diagonal_quad(l: f32, w: f32) -> MeshData {
    let a = Vec3::new(1.0, 1.0, 0.0).normalize();
    let p = Vec3::new(-1.0, 1.0, 0.0).normalize();
    let v = [-a * l - p * w, a * l - p * w, a * l + p * w, -a * l + p * w];
    MeshData {
        positions: v.iter().map(|x| x.to_array()).collect(),
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: vec![[0.0, 0.0]; 4],
        colors: vec![],
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// Regression: the bounding-sphere scale used the longest matrix column, which underestimates
/// sheared matrices (non-uniform parent scale × rotated child), so geometry crossing the frustum
/// edge was culled on every path.
#[test]
fn sheared_transforms_are_not_culled_while_visible() {
    let Some(mut app) = headless_app(256, 256) else { return };
    let best = best_path(&app);
    let quad = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(diagonal_quad(1.0, 0.1));
    let material = Material {
        double_sided: true,
        ..Material::unlit(Vec4::ONE)
    };
    // Parent scale (1, 4, 1) × child rotation 45° maps the quad's long axis to (0, 4, 0): the
    // quad reaches y = -4.75, inside the ortho frustum (bottom -5), although the longest column
    // (2.92) says the sphere ends below it.
    let py = -8.75;
    let parent = app
        .world
        .spawn((Transform::from_xyz(-2.0, py, 0.0).with_scale(Vec3::new(1.0, 4.0, 1.0)),));
    app.world.spawn((
        Transform::default().with_rotation(Quat::from_rotation_z(std::f32::consts::FRAC_PI_4)),
        Parent(parent),
        Mesh3d(quad),
        material.clone(),
    ));
    // The same sheared matrix through an explicit InstanceTransform buffer.
    let m = Mat4::from_translation(Vec3::new(2.0, py, 0.0))
        * Mat4::from_scale(Vec3::new(1.0, 4.0, 1.0))
        * Mat4::from_rotation_z(std::f32::consts::FRAC_PI_4);
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(
            vec![InstanceTransform::from_mat4(m)],
            BufferUsage::Dynamic,
        ));
    app.world.spawn((InstancedMesh {
        mesh: quad,
        material,
        instances: h,
    },));
    app.world.spawn((
        Transform::from_xyz(0.0, 0.0, 5.0),
        Camera3d {
            projection: Projection::Orthographic {
                height: 10.0,
                near: 0.1,
                far: 100.0,
            },
            ..Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0))
        },
    ));
    for path in gpu_paths_up_to(best) {
        set_path(&mut app, path);
        app.resources.expect_mut::<RenderSettings>().frustum_culling = false;
        let unculled = render_to_image(&mut app).expect("capture");
        app.resources.expect_mut::<RenderSettings>().frustum_culling = true;
        let culled = render_to_image(&mut app).expect("capture");
        let lit = |img: &ImageData, x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..256).map(move |y| (x, y)))
                .filter(|&(x, y)| img.get(x, y) != CLEAR)
                .count()
        };
        for (name, x0, x1) in [("ecs", 0, 128), ("explicit", 128, 256)] {
            let expected = lit(&unculled, x0, x1);
            assert!(expected > 10, "{path:?} {name}: quad tip visible without culling");
            assert_eq!(
                lit(&culled, x0, x1),
                expected,
                "{path:?} {name}: visible geometry culled"
            );
        }
    }
}

/// Regression: the CPU path culled an explicit buffer only as a whole (one box around every
/// instance), so a buffer spanning the world drew all its instances whenever any part of it
/// was on screen. Blocks of instances are culled individually now, and each visible run of
/// blocks is one draw.
#[test]
fn cpu_path_culls_explicit_buffers_per_block() {
    let Some(mut app) = headless_app(96, 64) else { return };
    set_path(&mut app, DrawPath::CpuDirect);
    let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.4));
    // Two 10 km rows starting at x = 0: instances 0.. and n/2.. are both next to the camera.
    let n = 100_000usize;
    let data = (0..n)
        .map(|i| {
            let k = i % (n / 2);
            let y = if i < n / 2 { 0.5 } else { -0.5 };
            InstanceCompact::new(
                Vec3::new(k as f32 * 0.2 - 1.0, y, 0.0),
                Quat::from_rotation_z(i as f32),
                1.0,
            )
        })
        .collect();
    let h = app
        .resources
        .expect_mut::<InstanceBuffers>()
        .insert(InstanceBuffer::from_vec(data, BufferUsage::Dynamic));
    app.world.spawn((InstancedMesh {
        mesh,
        material: Material::unlit(Vec4::ONE),
        instances: h,
    },));
    spawn_camera(&mut app, 6.0);
    let culled = render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert_eq!(s.instances, n as u64);
    assert!(
        s.instances_drawn > 0 && s.instances_drawn <= 4 * 4096,
        "only the blocks near the camera: {s:?}"
    );
    assert_eq!(s.culled + s.instances_drawn, n as u64);
    assert_eq!(s.draw_calls, 2, "one run at the start of each row: {s:?}");
    assert!(count_rgb(&culled, [255, 255, 255]) > 100);

    app.resources.expect_mut::<RenderSettings>().frustum_culling = false;
    let full = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instances_drawn, n as u64);
    assert_eq!(
        diff_fraction(&culled, &full, 0),
        0.0,
        "culling must not change the image"
    );
}

/// Regression: on the CPU path every changed ECS matrix was uploaded, although culled batches
/// draw nothing and partially visible ones draw from a separate buffer. Now only batches drawn
/// straight from the ECS buffer upload; the others keep their changes pending until they are.
#[test]
fn cpu_path_uploads_ecs_matrices_only_for_drawn_batches() {
    let build = |app: &mut App| {
        set_path(app, DrawPath::CpuDirect);
        let mesh = app.resources.expect_mut::<RenderAssets>().add_mesh(MeshData::cube(0.3));
        let mut entities = Vec::new();
        // Batch A (red) in front of the camera, batch B (green) behind it.
        for (z, color) in [
            (0.0, Vec4::new(1.0, 0.0, 0.0, 1.0)),
            (16.0, Vec4::new(0.0, 1.0, 0.0, 1.0)),
        ] {
            for i in 0..100 {
                let p = Vec3::new((i % 10) as f32 * 0.5 - 2.25, (i / 10) as f32 * 0.4 - 1.8, z);
                entities.push(app.world.spawn((
                    Transform::from_xyz(p.x, p.y, p.z),
                    Mesh3d(mesh),
                    Material::unlit(color),
                )));
            }
        }
        let cam = app.world.spawn((
            Transform::from_xyz(0.0, 0.0, 8.0),
            Camera3d::default().with_clear_color(Vec4::new(0.0, 0.0, 1.0, 1.0)),
        ));
        (entities, cam)
    };
    let Some(mut app) = headless_app(64, 64) else { return };
    let (entities, cam) = build(&mut app);
    render_to_image(&mut app).expect("capture");
    for frame in 0..3 {
        for &e in &entities {
            app.world.get::<&mut Transform>(e).unwrap().translation.y += 0.01;
        }
        render_to_image(&mut app).expect("capture");
        let s = stats(&app);
        assert_eq!((s.instances_drawn, s.culled), (100, 100), "{s:?}");
        assert_eq!(s.instance_bytes_uploaded, 100 * 64, "frame {frame}: only batch A");
    }
    // Turn around: B's pending matrices are uploaded now, A's are not needed.
    *app.world.get::<&mut Transform>(cam).unwrap() =
        Transform::from_xyz(0.0, 0.0, 8.0).looking_at(Vec3::new(0.0, 0.0, 20.0), Vec3::Y);
    let img = render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instance_bytes_uploaded, 100 * 64);
    assert!(count_rgb(&img, [0, 255, 0]) > 200, "batch B visible");

    // Identical to a scene built in its final state.
    let Some(mut fresh) = headless_app(64, 64) else { return };
    let (fresh_entities, fresh_cam) = build(&mut fresh);
    for (&e, &f) in entities.iter().zip(&fresh_entities) {
        *fresh.world.get::<&mut Transform>(f).unwrap() = *app.world.get::<&Transform>(e).unwrap();
    }
    *fresh.world.get::<&mut Transform>(fresh_cam).unwrap() = *app.world.get::<&Transform>(cam).unwrap();
    let expected = render_to_image(&mut fresh).expect("capture");
    assert_eq!(diff_fraction(&img, &expected, 0), 0.0);
}

/// A partially visible ECS batch large enough to be culled on all threads renders exactly like
/// the unculled scene, and a still frame afterwards uploads nothing.
#[test]
fn large_partially_visible_ecs_batch_renders_like_unculled() {
    let Some(mut app) = headless_app(96, 64) else { return };
    set_path(&mut app, DrawPath::CpuDirect);
    let mesh = app
        .resources
        .expect_mut::<RenderAssets>()
        .add_mesh(MeshData::cube(0.12));
    let n = 20_000;
    for i in 0..n {
        let p = Vec3::new((i % 200) as f32 * 0.15 - 4.0, (i / 200) as f32 * 0.15 - 7.5, 0.0);
        app.world.spawn((
            Transform::from_xyz(p.x, p.y, p.z),
            Mesh3d(mesh),
            Material::unlit(Vec4::new(1.0, 1.0, 0.0, 1.0)),
        ));
    }
    spawn_camera(&mut app, 8.0);
    let culled = render_to_image(&mut app).expect("capture");
    let s = stats(&app);
    assert!(s.instances_drawn > 1000 && s.instances_drawn < n as u64 / 2, "{s:?}");
    assert_eq!(s.culled + s.instances_drawn, n as u64);
    assert_eq!(s.draw_calls, 1);
    app.resources.expect_mut::<RenderSettings>().frustum_culling = false;
    let full = render_to_image(&mut app).expect("capture");
    assert_eq!(diff_fraction(&culled, &full, 0), 0.0);
    app.resources.expect_mut::<RenderSettings>().frustum_culling = true;
    render_to_image(&mut app).expect("capture");
    render_to_image(&mut app).expect("capture");
    assert_eq!(stats(&app).instance_bytes_uploaded, 0, "{:?}", stats(&app));
}
