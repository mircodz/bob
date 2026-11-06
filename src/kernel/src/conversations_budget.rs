use rusqlite::Connection;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const VM_STEP_LIMIT: i32 = 2_000_000;
const PROGRESS_INTERVAL: i32 = 1_000;
const EXHAUSTED: &str = "archive SQL work budget exhausted; retrieval is incomplete";

struct ProgressGuard<'a>(&'a Connection);

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

pub(super) fn run<T>(
    db: &Connection,
    work: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let exhausted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&exhausted);
    let mut remaining = VM_STEP_LIMIT;
    db.progress_handler(
        PROGRESS_INTERVAL,
        Some(move || {
            remaining = remaining.saturating_sub(PROGRESS_INTERVAL);
            if remaining <= 0 {
                signal.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        }),
    );
    let guard = ProgressGuard(db);
    let result = work();
    drop(guard);
    // Source projection may catch an interruption and return a partial preview.
    if exhausted.load(Ordering::Relaxed) {
        anyhow::bail!(EXHAUSTED);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn sum(db: &Connection, rows: i64) -> rusqlite::Result<i64> {
        db.query_row(
            "WITH RECURSIVE numbers(n) AS (
                SELECT 1
                UNION ALL SELECT n + 1 FROM numbers WHERE n < ?1
             ) SELECT sum(n) FROM numbers",
            [rows],
            |row| row.get(0),
        )
    }

    fn assert_unrestricted(db: &Connection) {
        assert_eq!(sum(db, 200_000).unwrap(), 20_000_100_000);
        db.execute_batch("CREATE TABLE recovery(value INTEGER); INSERT INTO recovery VALUES (7);")
            .unwrap();
        db.execute("UPDATE recovery SET value = value + 1", [])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM recovery", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            8
        );
    }

    #[test]
    fn expensive_select_exhausts_budget_and_connection_recovers() {
        let db = Connection::open_in_memory().unwrap();
        let error = run(&db, || Ok(sum(&db, 200_000)?)).unwrap_err();
        assert_eq!(error.to_string(), EXHAUSTED);
        assert_unrestricted(&db);
        assert_eq!(run(&db, || Ok(sum(&db, 10)?)).unwrap(), 55);
    }

    #[test]
    fn swallowed_interruption_cannot_return_a_preview() {
        let db = Connection::open_in_memory().unwrap();
        let error = run(&db, || {
            let interrupted = sum(&db, 200_000).unwrap_err();
            assert_eq!(
                interrupted.sqlite_error_code(),
                Some(rusqlite::ErrorCode::OperationInterrupted)
            );
            Ok("partial preview")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), EXHAUSTED);
        assert_unrestricted(&db);
    }

    #[test]
    fn budget_is_shared_across_statements() {
        let db = Connection::open_in_memory().unwrap();
        assert_eq!(run(&db, || Ok(sum(&db, 40_000)?)).unwrap(), 800_020_000);
        let error = run(&db, || {
            for _ in 0..10 {
                sum(&db, 40_000)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), EXHAUSTED);
    }

    #[test]
    fn normal_success_removes_handler() {
        let db = Connection::open_in_memory().unwrap();
        assert_eq!(run(&db, || Ok(sum(&db, 10)?)).unwrap(), 55);
        assert_unrestricted(&db);
    }

    #[test]
    fn ordinary_error_is_preserved_and_removes_handler() {
        let db = Connection::open_in_memory().unwrap();
        let error = run::<()>(&db, || anyhow::bail!("source unavailable")).unwrap_err();
        assert_eq!(error.to_string(), "source unavailable");
        assert_unrestricted(&db);
    }

    #[test]
    fn panic_removes_handler() {
        let db = Connection::open_in_memory().unwrap();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = run::<()>(&db, || {
                assert_eq!(sum(&db, 10)?, 55);
                panic!("source projection panicked");
            });
        }));
        assert!(panic.is_err());
        assert_unrestricted(&db);
    }

    #[test]
    fn transaction_and_savepoint_survive_select_interruption() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE saved(value INTEGER);
             BEGIN;
             INSERT INTO saved VALUES (1);
             SAVEPOINT retrieval;
             INSERT INTO saved VALUES (2);",
        )
        .unwrap();
        let error = run(&db, || Ok(sum(&db, 200_000)?)).unwrap_err();
        assert_eq!(error.to_string(), EXHAUSTED);
        assert!(!db.is_autocommit());
        assert_eq!(sum(&db, 200_000).unwrap(), 20_000_100_000);
        db.execute_batch(
            "ROLLBACK TO retrieval;
             INSERT INTO saved VALUES (3);
             RELEASE retrieval;
             COMMIT;",
        )
        .unwrap();
        assert!(db.is_autocommit());
        assert_eq!(
            db.query_row("SELECT sum(value) FROM saved", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }
}
