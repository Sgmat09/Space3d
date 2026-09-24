//! The engine keeps ticking while the database is busy.
#![cfg(not(target_arch = "wasm32"))]

use space3d_core::prelude::*;
use space3d_db::{Db, DbPlugin, DbStatus, Migration};
use space3d_tasks::AsyncQueue;
use std::time::{Duration, Instant};

/// Sum of 1..=1e6 via a recursive CTE: slow enough to span many frames.
const SLOW_QUERY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 1000000)
                          SELECT sum(x) AS total FROM c";

#[derive(Default)]
struct Ticks(u64);

struct SlowResult(i64);

#[test]
fn engine_ticks_while_slow_query_runs() {
    let mut app = App::new();
    app.add_plugin(DbPlugin::in_memory().with_migration(Migration::new(1, "scores", "CREATE TABLE scores(v INTEGER)")));
    app.init_resource::<Ticks>();
    app.add_system(Stage::FixedUpdate, |ctx| ctx.resources.expect_mut::<Ticks>().0 += 1);

    // Issue the slow query from a system on the first frame, the way game code would.
    let mut issued = false;
    app.add_system(Stage::Update, move |ctx| {
        if issued || !ctx.resources.expect::<DbStatus>().is_ready() {
            return;
        }
        issued = true;
        let task = ctx.resources.expect::<Db>().query::<i64>(SLOW_QUERY, ());
        ctx.resources.expect_mut::<AsyncQueue>().track(task, |ctx, result| {
            ctx.resources.insert(SlowResult(result.expect("slow query")[0]));
        });
    });

    let started = Instant::now();
    let mut frames = 0u64;
    let mut frames_waiting = 0u64;
    let mut slowest_frame = Duration::ZERO;
    while !app.resources.contains::<SlowResult>() {
        let t = Instant::now();
        app.step_ticks(1);
        slowest_frame = slowest_frame.max(t.elapsed());
        frames += 1;
        if app.resources.expect::<DbStatus>().is_ready() {
            frames_waiting += 1;
        }
        std::thread::sleep(Duration::from_millis(1));
        assert!(started.elapsed() < Duration::from_secs(60), "query never finished");
    }
    let query_time = started.elapsed();

    assert_eq!(app.resources.expect::<SlowResult>().0, 500_000_500_000);
    assert!(
        matches!(app.resources.expect::<DbStatus>(), DbStatus::Ready(r) if r.applied == [1]),
        "{:?}",
        app.resources.expect::<DbStatus>()
    );
    // The simulation advanced one fixed tick per frame the whole time.
    assert_eq!(app.resources.expect::<Ticks>().0, frames);
    assert_eq!(app.resources.expect::<Time>().tick, frames);
    assert!(
        frames_waiting >= 3,
        "expected several frames while the query ran, got {frames_waiting} in {query_time:?}"
    );
    // No frame was stalled by the query.
    assert!(
        slowest_frame < query_time / 2,
        "slowest frame {slowest_frame:?} vs query {query_time:?}"
    );
}
