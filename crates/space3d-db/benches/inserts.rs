//! 10k inserts in one transaction, in-memory and file-backed (WAL).

use criterion::{Criterion, criterion_group, criterion_main};
use space3d_db::{Db, Result};

fn insert_10k(db: &Db) -> Result<()> {
    db.blocking().transaction(|tx| {
        tx.execute("DELETE FROM items", ())?;
        for i in 0..10_000i64 {
            tx.execute(
                "INSERT INTO items(name, score, payload) VALUES (?1, ?2, ?3)",
                (format!("item-{i}"), i, vec![0u8; 16]),
            )?;
        }
        Ok(())
    })
}

fn bench(c: &mut Criterion) {
    const SCHEMA: &str =
        "CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, name TEXT, score INTEGER, payload BLOB)";

    let mem = Db::open_in_memory();
    mem.blocking().execute_batch(SCHEMA).unwrap();
    c.bench_function("db/10k_inserts_one_tx/memory", |b| b.iter(|| insert_10k(&mem).unwrap()));

    let dir = tempfile::tempdir().unwrap();
    let file = Db::open(dir.path().join("bench.db"));
    file.blocking().execute_batch(SCHEMA).unwrap();
    c.bench_function("db/10k_inserts_one_tx/file_wal", |b| {
        b.iter(|| insert_10k(&file).unwrap())
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench
}
criterion_main!(benches);
