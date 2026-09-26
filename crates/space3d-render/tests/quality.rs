//! `GraphicsQuality` on a real renderer (graphics spec §4.4, Phase I): every tier applies to a
//! lit, shadowed, post-processed scene — MSAA, render scale, bloom chain, shadow map, shading
//! model — renders it without GPU errors, and its prewarm request builds the pipelines the next
//! frame uses. Skips without a GPU adapter.

use space3d_core::prelude::*;
use space3d_render::*;

fn app() -> Option<App> {
    let mut app = App::new();
    app.add_plugin(RenderPlugin::default());
    match pollster::block_on(Renderer::new_headless(320, 180)) {
        Ok(r) => {
            app.insert_resource(r);
            Some(app)
        }
        Err(e) => {
            eprintln!("skipping GPU test: no adapter ({e})");
            None
        }
    }
}

fn scene(app: &mut App) -> (Entity, Entity, Vec<Material>) {
    let assets = app.resources.expect_mut::<RenderAssets>();
    let cube = assets.add_mesh(MeshData::cube(1.0));
    let ground = assets.add_mesh(MeshData::plane(20.0));
    let materials = vec![
        Material::srgb(0.8, 0.3, 0.2).with_metallic_roughness(0.2, 0.5),
        Material::color(Vec4::new(0.5, 0.5, 0.55, 1.0)).with_emissive(Vec3::new(3.0, 1.0, 0.2)),
        Material::additive(Vec4::new(0.2, 0.6, 1.0, 1.0)),
    ];
    for (i, m) in materials.iter().enumerate() {
        app.world.spawn((
            Transform::from_xyz(i as f32 * 1.6 - 1.6, 0.5, 0.0),
            Mesh3d(cube),
            m.clone(),
        ));
    }
    app.world
        .spawn((Transform::IDENTITY, Mesh3d(ground), Material::srgb(0.4, 0.4, 0.4)));
    let camera = app.world.spawn((
        Transform::from_xyz(0.0, 6.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
        Camera3d::default().with_clear_color(Vec4::new(0.01, 0.01, 0.02, 1.0)),
        PostProcess::default().with_chromatic_aberration(0.2),
    ));
    let sun = app.world.spawn((
        Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.3), Vec3::Y),
        DirectionalLight::default(),
        DirectionalShadow::new().with_fit(ShadowFit::Bounds(Aabb::new(
            Vec3::new(-10.0, -1.0, -10.0),
            Vec3::new(10.0, 3.0, 10.0),
        ))),
    ));
    (camera, sun, materials)
}

#[test]
fn every_tier_applies_and_renders() {
    let Some(mut app) = app() else { return };
    let (camera, sun, materials) = scene(&mut app);
    let recommended = GraphicsQuality::recommended_for(app.resources.expect::<Renderer>());
    eprintln!("recommended tier: {recommended}");
    for q in GraphicsQuality::ALL {
        q.apply(&mut app.world, &mut app.resources);
        let img = render_to_image(&mut app).expect("capture");
        assert_eq!((img.width, img.height), (320, 180));
        let r = app.resources.expect::<Renderer>();
        assert_eq!(r.gpu_error_count(), 0, "{q}");
        assert_eq!(r.msaa_samples(), q.msaa_samples(r.capabilities()), "{q}");
        let frame = app.resources.expect::<FrameSettings>();
        assert_eq!(frame.render_scale, if q == GraphicsQuality::Low { 0.75 } else { 1.0 });
        let lighting = app.resources.expect::<LightingSettings>();
        assert_eq!(lighting.shading, q.lighting(LightingSettings::default()).shading);
        let post = *app.world.get::<&PostProcess>(camera).unwrap();
        assert_eq!(
            post.bloom.mip_count,
            q.post(PostProcess::default()).bloom.mip_count,
            "{q}"
        );
        assert_eq!(post.chromatic_aberration.enabled, q != GraphicsQuality::Low, "{q}");
        let shadow = *app.world.get::<&DirectionalShadow>(sun).unwrap();
        assert_eq!(shadow.map_size, q.shadow(DirectionalShadow::default()).map_size);
        let stats = app.resources.expect::<RenderStats>();
        assert_eq!(stats.depth_views, 1, "{q}: the shadow map");
        let lit = img.pixels.chunks(4).filter(|p| p[0].max(p[1]).max(p[2]) > 30).count();
        assert!(lit > 3000, "{q}: {lit} lit pixels");

        // The tier's prewarm plan, after a frame of the scene: nothing is left to build for it.
        let request = q.prewarm_request(&materials);
        app.resources.expect_mut::<Renderer>().prewarm(&request);
        render_to_image(&mut app).expect("capture");
        assert_eq!(app.resources.expect::<Renderer>().gpu_error_count(), 0, "{q}");
    }
}

/// Under `DownlevelOverride::webgl2()` the device recommends Low (no compute, no float targets).
#[test]
fn webgl2_recommends_low() {
    let Some(mut app) = app() else { return };
    app.insert_resource(FrameSettings::default().with_downlevel(DownlevelOverride::webgl2()));
    scene(&mut app);
    render_to_image(&mut app).expect("capture");
    let r = app.resources.expect::<Renderer>();
    assert!(!r.graphics_caps().compute && r.graphics_caps().hdr_format.is_none());
    assert_eq!(GraphicsQuality::recommended_for(r), GraphicsQuality::Low);
}
