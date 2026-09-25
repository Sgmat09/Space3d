//! SQL layer: migrations, transactions, row mapping, file databases.
#![cfg(not(target_arch = "wasm32"))]

use serde::Deserialize;
use space3d_db::{Db, DbConfig, DbError, Migration, Value, params};

const MIGRATIONS: &[Migration] = &[
    Migration::new(
        1,
        "create_players",
        "CREATE TABLE players(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, level INTEGER NOT NULL DEFAULT 1);",
    ),
    Migration::new(
        2,
        "create_items",
        "CREATE TABLE items(id INTEGER PRIMARY KEY, player INTEGER NOT NULL REFERENCES players(id), name TEXT NOT NULL);
         CREATE INDEX items_by_player ON items(player);",
    ),
];

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/game.db");
    {
        let db = Db::open(&path);
        let r = db.blocking().migrate(MIGRATIONS.iter().cloned()).unwrap();
        assert_eq!(r.applied, vec![1, 2]);
        assert_eq!(r.current_version, Some(2));
        // Same handle, same list: nothing to do.
        let r = db.blocking().migrate(MIGRATIONS.iter().cloned()).unwrap();
        assert!(r.applied.is_empty());
        db.blocking()
            .execute("INSERT INTO players(name) VALUES (?1)", ("ada",))
            .unwrap();
    }
    // Reopen: schema and data persisted, migrations not re-run, new ones applied.
    let db = Db::open(&path);
    let mut all = MIGRATIONS.to_vec();
    all.push(Migration::owned(
        3,
        "add_xp",
        "ALTER TABLE players ADD COLUMN xp INTEGER NOT NULL DEFAULT 0;",
    ));
    let r = db.blocking().migrate(all.clone()).unwrap();
    assert_eq!(r.applied, vec![3]);
    assert_eq!(r.current_version, Some(3));
    assert!(db.blocking().migrate(all).unwrap().applied.is_empty());
    let recorded: Vec<(i64, String)> = db
        .blocking()
        .query("SELECT version, name FROM schema_migrations ORDER BY version", ())
        .unwrap();
    assert_eq!(recorded.len(), 3);
    assert_eq!(recorded[2], (3, "add_xp".to_string()));
    let names: Vec<String> = db.blocking().query("SELECT name FROM players", ()).unwrap();
    assert_eq!(names, ["ada"]);

    // File databases run in WAL mode.
    let mode: Option<String> = db.blocking().query_one("PRAGMA journal_mode", ()).unwrap();
    assert_eq!(mode.as_deref(), Some("wal"));
}

#[test]
fn failing_migration_rolls_back_and_reports() {
    let db = Db::open_in_memory();
    let bad = [
        Migration::new(1, "ok", "CREATE TABLE a(x INTEGER);"),
        Migration::new(2, "broken", "CREATE TABLE b(x INTEGER); THIS IS NOT SQL;"),
    ];
    let err = db.blocking().migrate(bad).unwrap_err();
    assert!(matches!(err, DbError::Migration { version: 2, .. }), "{err}");
    // Migration 1 committed, migration 2 fully rolled back (table b absent).
    let tables: Vec<String> = db
        .blocking()
        .query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('a', 'b') ORDER BY name",
            (),
        )
        .unwrap();
    assert_eq!(tables, ["a"]);

    let dup = [Migration::new(1, "x", "SELECT 1"), Migration::new(1, "y", "SELECT 1")];
    assert!(matches!(db.blocking().migrate(dup), Err(DbError::Migration { .. })));
}

#[test]
fn transaction_rolls_back_on_error() {
    let db = Db::open_in_memory();
    db.blocking().migrate(MIGRATIONS.iter().cloned()).unwrap();

    let err = db
        .transaction(|tx| {
            tx.execute("INSERT INTO players(name) VALUES (?1)", ("ghost",))?;
            tx.execute("INSERT INTO players(name) VALUES (?1)", ("ghost2",))?;
            Err::<(), _>(DbError::other("abort!"))
        })
        .block()
        .unwrap()
        .unwrap_err();
    assert_eq!(err, DbError::Other("abort!".into()));

    // A SQL error mid-transaction also rolls back the earlier statements.
    let err = db
        .transaction(|tx| {
            tx.execute("INSERT INTO players(name) VALUES ('dup')", ())?;
            tx.execute("INSERT INTO players(name) VALUES ('dup')", ())?; // UNIQUE violation
            Ok(())
        })
        .block()
        .unwrap()
        .unwrap_err();
    assert!(err.is_constraint_violation(), "{err:?}");

    // Panics are contained, roll back, and the worker keeps going.
    let err = db
        .transaction(|tx| -> space3d_db::Result<()> {
            tx.execute("INSERT INTO players(name) VALUES ('panicky')", ())?;
            panic!("boom");
        })
        .block()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, DbError::Panicked(ref m) if m == "boom"));

    let count: Option<i64> = db.blocking().query_one("SELECT count(*) FROM players", ()).unwrap();
    assert_eq!(count, Some(0));

    // And a successful transaction commits.
    let id = db
        .blocking()
        .transaction(|tx| {
            tx.execute("INSERT INTO players(name, level) VALUES (?1, ?2)", ("ada", 7))?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO items(player, name) VALUES (?1, ?2)", params![id, "sword"])?;
            Ok(id)
        })
        .unwrap();
    let items: Vec<(i64, String)> = db.blocking().query("SELECT player, name FROM items", ()).unwrap();
    assert_eq!(items, [(id, "sword".to_string())]);
}

#[derive(Debug, Deserialize, PartialEq)]
struct Player {
    id: i64,
    name: String,
    level: u32,
    active: bool,
    nickname: Option<String>,
    avatar: Option<Vec<u8>>,
    stats: Stats,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Stats {
    str_: i32,
    tags: Vec<String>,
}

#[test]
fn query_maps_rows_via_serde() {
    let db = Db::open_in_memory();
    let b = db.blocking();
    b.execute_batch(
        "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT, level INTEGER, active INTEGER,
                        nickname TEXT, avatar BLOB, stats TEXT)",
    )
    .unwrap();
    let stats = Value::json(&serde_json::json!({"str_": 12, "tags": ["tank"]})).unwrap();
    b.execute(
        "INSERT INTO p(name, level, active, nickname, avatar, stats) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params!["ada", 3, true, None::<String>, vec![1u8, 2, 3], stats.clone()],
    )
    .unwrap();
    b.execute(
        "INSERT INTO p(name, level, active, nickname, avatar, stats) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params!["bob", 1, false, "bobby", None::<Vec<u8>>, stats],
    )
    .unwrap();

    let players: Vec<Player> = db.query("SELECT * FROM p ORDER BY id", ()).block().unwrap().unwrap();
    assert_eq!(
        players[0],
        Player {
            id: 1,
            name: "ada".into(),
            level: 3,
            active: true,
            nickname: None,
            avatar: Some(vec![1, 2, 3]),
            stats: Stats {
                str_: 12,
                tags: vec!["tank".into()],
            },
        }
    );
    assert_eq!(players[1].nickname.as_deref(), Some("bobby"));
    assert!(!players[1].active);

    // Scalars, tuples, query_one, raw rows.
    let n: Option<i64> = b.query_one("SELECT count(*) FROM p WHERE level > ?1", (0,)).unwrap();
    assert_eq!(n, Some(2));
    let none: Option<String> = b.query_one("SELECT name FROM p WHERE id = 99", ()).unwrap();
    assert_eq!(none, None);
    let pairs: Vec<(String, u32)> = b.query("SELECT name, level FROM p ORDER BY name", ()).unwrap();
    assert_eq!(pairs, [("ada".into(), 3), ("bob".into(), 1)]);
    let raw = b.query_raw("SELECT name, level FROM p WHERE id = ?1", [1]).unwrap();
    assert_eq!(raw.columns, ["name", "level"]);
    assert_eq!(raw.iter().next().unwrap().get("level"), Some(&Value::Integer(3)));

    // Errors are reported, not panicked.
    assert!(matches!(
        b.query::<i64>("SELECT nope FROM p", ()),
        Err(DbError::Sqlite { .. })
    ));
    assert!(matches!(
        b.query::<Player>("SELECT id FROM p", ()),
        Err(DbError::Serde(_))
    ));
    // execute() reports changed rows.
    assert_eq!(b.execute("UPDATE p SET level = level + 1", ()).unwrap(), 2);
}

#[test]
fn open_failure_is_reported_per_call() {
    let dir = tempfile::tempdir().unwrap();
    // A directory is not a database file.
    let db = Db::open_with(DbConfig::new(dir.path()));
    let err = db.execute("SELECT 1", ()).block().unwrap().unwrap_err();
    assert!(matches!(err, DbError::Open(_)), "{err:?}");
    assert!(matches!(
        db.kv("x").get::<i32>("k").block().unwrap(),
        Err(DbError::Open(_))
    ));
}

#[test]
fn ordering_and_raw_connection_access() {
    let db = Db::open_in_memory();
    // Fire-and-forget writes are applied in order before later reads.
    let _ = db.execute_batch("CREATE TABLE log(x INTEGER)");
    for i in 0..100 {
        let _ = db.execute("INSERT INTO log(x) VALUES (?1)", (i,));
    }
    let sum: Vec<i64> = db.query("SELECT sum(x) FROM log", ()).block().unwrap().unwrap();
    assert_eq!(sum, [4950]);
    let version = db
        .with_connection(|c| Ok(c.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0))?))
        .block()
        .unwrap()
        .unwrap();
    assert!(version.starts_with('3'));
}

#[test]
fn queue_depth_counts_jobs_until_they_finish() {
    let db = Db::open_in_memory();
    db.blocking().execute_batch("CREATE TABLE t(x INTEGER);").unwrap();
    let tasks: Vec<_> = (0..50)
        .map(|i| db.execute("INSERT INTO t(x) VALUES (?1)", (i,)))
        .collect();
    assert!(db.queue_depth() <= 50);
    for t in tasks {
        t.block().unwrap().unwrap();
    }
    // The counter drops right after each job ran (just before its result is sent).
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.queue_depth() != 0 && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(db.queue_depth(), 0);
}
