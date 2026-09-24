//! Store behaviour, run against both the SQLite backend and the web backend
//! (over in-memory storage) so the two implementations stay in lock-step.

use crate::*;
use serde::{Deserialize, Serialize};
use space3d_tasks::Task;

fn get<T>(t: Task<Result<T>>) -> T {
    t.block().expect("task cancelled").expect("db error")
}

fn backends() -> [(&'static str, Db); 2] {
    [("sqlite", Db::open_in_memory()), ("web", Db::open_web_memory())]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Settings {
    volume: f32,
    name: String,
    keys: Vec<String>,
}

#[test]
fn kv_roundtrip() {
    for (backend, db) in backends() {
        let kv = db.kv("settings");
        let other = db.kv("settings:extra"); // must not collide with "settings"
        let s = Settings {
            volume: 0.5,
            name: "p1".into(),
            keys: vec!["W".into(), "A".into()],
        };
        assert_eq!(get(kv.get::<Settings>("audio")), None, "{backend}");
        get(kv.set("audio", &s));
        get(kv.set("zeta", &1u32));
        get(other.set("audio", &"shadow"));
        assert_eq!(get(kv.get::<Settings>("audio")), Some(s.clone()), "{backend}");
        assert!(get(kv.contains("zeta")), "{backend}");
        assert_eq!(get(kv.keys()), vec!["audio", "zeta"], "{backend}");

        // overwrite
        let s2 = Settings { volume: 1.0, ..s };
        get(kv.set("audio", &s2));
        assert_eq!(get(kv.get::<Settings>("audio")), Some(s2), "{backend}");

        // wrong type is a serde error, not a panic
        assert!(matches!(
            kv.get::<u32>("audio").block().unwrap(),
            Err(DbError::Serde(_))
        ));

        assert!(get(kv.delete("audio")), "{backend}");
        assert!(!get(kv.delete("audio")), "{backend}");
        assert_eq!(get(kv.clear()), 1, "{backend}");
        assert!(get(kv.keys()).is_empty(), "{backend}");
        assert_eq!(
            get(other.get::<String>("audio")).as_deref(),
            Some("shadow"),
            "{backend}"
        );
    }
}

#[test]
fn save_slots_list_order() {
    for (backend, db) in backends() {
        let saves = db.save_slots();
        let png = vec![0x89, b'P', b'N', b'G'];
        get(saves.save("a", NewSave::new("First", b"aaa".to_vec())));
        get(saves.save(
            "b",
            NewSave::from_state(
                "Second",
                &Settings {
                    volume: 0.1,
                    name: "b".into(),
                    keys: vec![],
                },
            )
            .unwrap()
            .with_metadata(serde_json::json!({"level": 3}))
            .with_thumbnail(png.clone()),
        ));
        get(saves.save("c", NewSave::new("Third", b"c".to_vec())));
        let order: Vec<String> = get(saves.list()).into_iter().map(|m| m.slot).collect();
        assert_eq!(order, ["c", "b", "a"], "{backend}: most recent first");

        // Re-saving an old slot moves it to the front and keeps created_at.
        let first = get(saves.list()).into_iter().find(|m| m.slot == "a").unwrap();
        let resaved = get(saves.save("a", NewSave::new("First again", b"aaaa".to_vec())));
        assert_eq!(resaved.created_at, first.created_at, "{backend}");
        assert!(resaved.updated_at >= first.updated_at, "{backend}");
        let list = get(saves.list());
        let order: Vec<&str> = list.iter().map(|m| m.slot.as_str()).collect();
        assert_eq!(order, ["a", "c", "b"], "{backend}");
        assert_eq!(list[0].label, "First again");
        assert_eq!(list[0].size_bytes, 4);
        assert_eq!(
            list[0].metadata,
            serde_json::json!({}),
            "{backend}: null metadata stored as {{}}"
        );

        let b = get(saves.load("b")).unwrap();
        assert_eq!(b.meta.metadata["level"], 3, "{backend}");
        assert!(b.meta.has_thumbnail);
        assert_eq!(b.thumbnail_png.as_deref(), Some(&png[..]));
        assert_eq!(b.decode::<Settings>().unwrap().name, "b");
        assert_eq!(get(saves.thumbnail("b")), Some(png), "{backend}");
        assert_eq!(get(saves.thumbnail("a")), None, "{backend}");

        assert!(get(saves.delete("c")), "{backend}");
        assert!(!get(saves.delete("c")), "{backend}");
        assert!(get(saves.load("c")).is_none(), "{backend}");
        assert_eq!(get(saves.list()).len(), 2, "{backend}");
    }
}

#[test]
fn leaderboard_ranking() {
    for (backend, db) in backends() {
        let board = db.leaderboard("arcade");
        assert!(get(board.submit("alice", 100)).improved);
        get(board.submit("bob", 300));
        get(board.submit("carol", 200));
        get(board.submit("dave", 200));
        let s = get(board.submit("alice", 50));
        assert!(!s.improved, "{backend}: worse score is ignored");
        assert_eq!((s.best.score, s.best.rank), (100, 4), "{backend}");
        let s = get(board.submit("alice", 400));
        assert!(s.improved);
        assert_eq!(s.best.rank, 1, "{backend}");

        let top: Vec<(u64, String, i64)> = get(board.top(10))
            .into_iter()
            .map(|e| (e.rank, e.player, e.score))
            .collect();
        assert_eq!(
            top,
            vec![
                (1, "alice".into(), 400),
                (2, "bob".into(), 300),
                (3, "carol".into(), 200), // tie: carol submitted first
                (3, "dave".into(), 200),
            ],
            "{backend}"
        );
        assert_eq!(get(board.top(2)).len(), 2);
        assert_eq!(get(board.rank("dave")).map(|e| e.rank), Some(3), "{backend}");
        assert_eq!(get(board.rank("nobody")), None);
        assert_eq!(get(board.count()), 4);

        // Other boards are independent; lower-is-better boards rank ascending.
        let speedrun = db.leaderboard("speedrun").with_order(ScoreOrder::LowerIsBetter);
        get(speedrun.submit("alice", 90_000));
        get(speedrun.submit("bob", 80_000));
        assert!(!get(speedrun.submit("bob", 85_000)).improved);
        assert_eq!(get(speedrun.top(1))[0].player, "bob", "{backend}");
        assert_eq!(get(speedrun.count()), 2);

        assert!(get(board.remove("bob")));
        assert_eq!(get(board.rank("carol")).unwrap().rank, 2, "{backend}");
        assert_eq!(get(board.clear()), 3);
        assert!(get(board.top(10)).is_empty());
    }
}

#[test]
fn web_backend_rejects_sql() {
    let db = Db::open_web_memory();
    assert_eq!(db.backend(), BackendKind::Memory);
    assert!(!db.supports_sql());
    assert!(matches!(
        db.execute("SELECT 1", ()).block().unwrap(),
        Err(DbError::Unsupported(_))
    ));
    assert!(matches!(
        db.query::<i64>("SELECT 1", ()).block().unwrap(),
        Err(DbError::Unsupported(_))
    ));
    assert!(matches!(
        db.transaction(|_| Ok(())).block().unwrap(),
        Err(DbError::Unsupported(_))
    ));
    let report = get(db.migrate([Migration::new(1, "x", "CREATE TABLE x(a)")]));
    assert_eq!(report, MigrationReport::default());
}
