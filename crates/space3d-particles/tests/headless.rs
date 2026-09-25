//! Particles render headless: `render_to_image` and `Renderer::capture` include them, a capture
//! twice without ticks in between is identical, and nothing touches a window.

mod common;
use common::*;
use space3d_core::prelude::*;
use space3d_particles::ParticleBackend;
use space3d_particles::prelude::*;

const SPACE: Vec4 = Vec4::new(0.004, 0.005, 0.012, 1.0);

#[test]
fn render_to_image_includes_particles() {
    let Some(mut app) = app(320, 180) else { return };
    camera(&mut app, Vec3::new(0.0, 160.0, 220.0), Vec3::ZERO, SPACE);
    let empty = render(&mut app);
    app.spawn_effect(builtin::EXPLOSION, EffectSpawn::at(Vec3::ZERO).seed(7));
    app.step_ticks(12);
    let img = render(&mut app);
    dump(&img, "headless_explosion");
    let bg = empty.get(0, 0);
    let lit = fraction_not(&img, [bg[0], bg[1], bg[2]], 4);
    assert!(lit > 0.05, "{lit:.3} of the image lit by the explosion");
    let s = stats(&app);
    assert!(s.alive > 50, "{s:?}");
    assert_eq!(s.backend, Some(ParticleBackend::Cpu));
    // The app rendered every tick (the renderer is in the frame loop): nothing left to replay.
    assert!(s.draw_calls > 0 && s.ticks_simulated == 0, "{s:?}");
    // Rendering again without ticks replays nothing and gives the same image.
    let again = render(&mut app);
    assert_eq!(img, again);
    assert_eq!(stats(&app).ticks_simulated, 0);
    assert_eq!(gpu_errors(&app), 0);
}
