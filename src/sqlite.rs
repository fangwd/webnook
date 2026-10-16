//! Optional SQLite access (the `sqlite` feature).
//!
//! One connection lives on a dedicated worker thread, so blocking database
//! work never runs on async executor threads. Calls queue in a bounded
//! channel; when it is full they fail fast with [`Error::SqliteBusy`] instead
//! of piling up.
//!
//! In WAL mode the database consists of the main file plus `-wal` and `-shm`
//! files. Back it up with `VACUUM INTO` through [`Sqlite::call`] or the
//! `sqlite3 .backup` command rather than copying the main file alone.

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};

/// Options for opening a SQLite database.
#[derive(Debug, Clone)]
pub struct SqliteOptions {
    /// Use WAL journaling with `synchronous=NORMAL`. Default: `true`.
    pub wal: bool,
    /// How long SQLite waits on a lock held by another process. Default: 5s.
    pub busy_timeout: Duration,
    /// Maximum number of calls waiting for the worker. Default: 64.
    pub queue_capacity: usize,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        Self {
            wal: true,
            busy_timeout: Duration::from_secs(5),
            queue_capacity: 64,
        }
    }
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A cloneable handle to a SQLite connection running on its own thread.
///
/// ```no_run
/// # async fn run() -> webnook::Result<()> {
/// use webnook::{Sqlite, SqliteOptions};
///
/// let db = Sqlite::open("site.db", SqliteOptions::default())?;
/// db.migrate(&["CREATE TABLE contacts (email TEXT NOT NULL, message TEXT NOT NULL);"])
///     .await?;
/// db.call(|conn| {
///     conn.execute(
///         "INSERT INTO contacts (email, message) VALUES (?1, ?2)",
///         ("ada@example.com", "Hello"),
///     )
/// })
/// .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Sqlite {
    jobs: mpsc::Sender<Job>,
}

impl Sqlite {
    /// Open (or create) a database at `path`, enforce foreign keys, apply
    /// `options`, and start the worker thread. The worker stops once every
    /// handle is dropped and queued calls have run.
    pub fn open(path: impl AsRef<Path>, options: SqliteOptions) -> Result<Self> {
        let mut conn = Connection::open(path).map_err(Error::Sqlite)?;
        configure(&conn, &options).map_err(Error::Sqlite)?;

        let (jobs, mut queue) = mpsc::channel::<Job>(options.queue_capacity.max(1));
        std::thread::Builder::new()
            .name("webnook-sqlite".into())
            .spawn(move || {
                while let Some(job) = queue.blocking_recv() {
                    job(&mut conn);
                }
            })?;
        Ok(Self { jobs })
    }

    /// Run `f` on the worker thread and return its result.
    ///
    /// `f` gets `&mut Connection`, so it can open transactions with
    /// [`Connection::transaction`]. Fails with [`Error::SqliteBusy`] when the
    /// queue is full, and with [`Error::SqlitePanicked`] if `f` panics (any
    /// open transaction is rolled back and the worker keeps running).
    pub async fn call<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(conn)));
            let _ = reply.send(result);
        });
        self.jobs.try_send(job).map_err(|err| match err {
            TrySendError::Full(_) => Error::SqliteBusy,
            TrySendError::Closed(_) => Error::SqliteClosed,
        })?;
        match response.await {
            Ok(Ok(result)) => result.map_err(Error::Sqlite),
            Ok(Err(_)) => Err(Error::SqlitePanicked),
            Err(_) => Err(Error::SqliteClosed),
        }
    }

    /// Apply schema migrations deterministically at startup.
    ///
    /// `migrations` is the full, append-only list of migration scripts. The
    /// database's `PRAGMA user_version` records how many have been applied;
    /// each pending script runs in its own transaction together with the
    /// version bump. Never edit or reorder applied scripts; append new ones.
    pub async fn migrate(&self, migrations: &[&str]) -> Result<()> {
        let migrations: Vec<String> = migrations.iter().map(|sql| sql.to_string()).collect();
        let known = migrations.len();
        let applied = self
            .call(move |conn| {
                let applied: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                let applied = usize::try_from(applied).unwrap_or(usize::MAX);
                for (index, sql) in migrations.iter().enumerate().skip(applied) {
                    let tx = conn.transaction()?;
                    tx.execute_batch(sql)?;
                    tx.pragma_update(None, "user_version", (index + 1) as i64)?;
                    tx.commit()?;
                    tracing::info!(version = index + 1, "applied sqlite migration");
                }
                Ok(applied)
            })
            .await?;
        if applied > known {
            return Err(Error::SqliteSchemaAhead {
                database: applied,
                known,
            });
        }
        Ok(())
    }
}

fn configure(conn: &Connection, options: &SqliteOptions) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(options.busy_timeout)?;
    if options.wal {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
    }
    Ok(())
}
