//! SQLite helper tests (requires the `sqlite` feature).

use std::sync::mpsc;
use std::time::Duration;

use tempfile::TempDir;
use webnook::{Error, Sqlite, SqliteOptions};

fn open(options: SqliteOptions) -> (TempDir, Sqlite) {
    let dir = TempDir::new().unwrap();
    let db = Sqlite::open(dir.path().join("site.db"), options).unwrap();
    (dir, db)
}

const V1: &str = "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);";
const V2: &str = "ALTER TABLE items ADD COLUMN note TEXT;";

#[tokio::test]
async fn migrations_apply_once_in_order() {
    let (dir, db) = open(SqliteOptions::default());
    db.migrate(&[V1]).await.unwrap();
    db.migrate(&[V1]).await.unwrap();
    db.migrate(&[V1, V2]).await.unwrap();

    db.call(|conn| conn.execute("INSERT INTO items (name, note) VALUES ('a', 'b')", []))
        .await
        .unwrap();
    let version: i64 = db
        .call(|conn| conn.query_row("PRAGMA user_version", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(version, 2);

    // Reopening sees the persisted version.
    drop(db);
    let db = Sqlite::open(dir.path().join("site.db"), SqliteOptions::default()).unwrap();
    db.migrate(&[V1, V2]).await.unwrap();
    let err = db.migrate(&[V1]).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::SqliteSchemaAhead {
                database: 2,
                known: 1
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn failed_migration_rolls_back() {
    let (_dir, db) = open(SqliteOptions::default());
    let err = db
        .migrate(&[V1, "CREATE TABLE ok (id INTEGER); NOT VALID SQL;"])
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Sqlite(_)), "{err}");

    let (version, partial): (i64, i64) = db
        .call(|conn| {
            let version = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let partial = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'ok'",
                [],
                |r| r.get(0),
            )?;
            Ok((version, partial))
        })
        .await
        .unwrap();
    assert_eq!((version, partial), (1, 0));
}

#[tokio::test]
async fn transactions_are_available() {
    let (_dir, db) = open(SqliteOptions::default());
    db.migrate(&[V1]).await.unwrap();

    let result = db
        .call(|conn| {
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO items (name) VALUES ('kept?')", [])?;
            tx.execute("INSERT INTO items (name) VALUES (NULL)", [])?;
            tx.commit()
        })
        .await;
    assert!(result.is_err());

    let count: i64 = db
        .call(|conn| conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(count, 0, "failed transaction must roll back");
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let (_dir, db) = open(SqliteOptions::default());
    db.migrate(&[
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES parent(id));",
    ])
    .await
    .unwrap();

    let result = db
        .call(|conn| conn.execute("INSERT INTO child (parent_id) VALUES (999)", []))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn full_queue_fails_fast() {
    let (_dir, db) = open(SqliteOptions {
        queue_capacity: 1,
        ..SqliteOptions::default()
    });

    let (started_tx, started) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let running = tokio::spawn({
        let db = db.clone();
        async move {
            db.call(move |_| {
                started_tx.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
            .await
        }
    });
    tokio::task::spawn_blocking(move || started.recv().unwrap())
        .await
        .unwrap();

    // The first probe takes the single queue slot and waits; the next one
    // must be rejected immediately.
    let err = loop {
        match tokio::time::timeout(Duration::from_millis(50), db.call(|_| Ok(()))).await {
            Ok(Err(err)) => break err,
            Ok(Ok(())) => panic!("the worker is blocked"),
            Err(_) => continue,
        }
    };
    assert!(err.is_busy(), "{err}");

    release.send(()).unwrap();
    running.await.unwrap().unwrap();
    let one: i64 = db
        .call(|conn| conn.query_row("SELECT 1", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(one, 1);
}

#[tokio::test]
async fn panicking_call_does_not_stop_the_worker() {
    let (_dir, db) = open(SqliteOptions::default());
    let err = db
        .call(|_| -> rusqlite::Result<()> { panic!("boom") })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SqlitePanicked), "{err}");

    let one: i64 = db
        .call(|conn| conn.query_row("SELECT 1", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(one, 1);
}

#[tokio::test]
async fn wal_mode_is_enabled() {
    let (_dir, db) = open(SqliteOptions::default());
    let mode: String = db
        .call(|conn| conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(mode, "wal");
}
