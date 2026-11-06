use crate::store::tests::fixture;
use crate::store::Store;
use crate::{canonical_database_path, SessionClaim};
use rusqlite::Connection;
use std::sync::{Arc, Barrier};

#[test]
fn canonical_missing_parent_identity_does_not_create_directories() {
    let (dir, store, _, _) = fixture();
    let missing = dir.0.join("not-created/nested/sessions.sqlite");
    let resolved = canonical_database_path(&missing).unwrap();
    assert_eq!(
        resolved,
        std::fs::canonicalize(&dir.0)
            .unwrap()
            .join("not-created/nested/sessions.sqlite")
    );
    assert!(!dir.0.join("not-created").exists());
    drop(store);
}

#[test]
fn schema_upgrade_requires_exclusive_compatibility_guard() {
    let (dir, store, root, _) = fixture();
    let path = dir.0.join("lab.db");
    let claim = SessionClaim::acquire(&path, root.session).unwrap();
    store.db.pragma_update(None, "user_version", 9).unwrap();
    let error = Store::open(&path).err().unwrap();
    assert!(
        error.to_string().contains("exclusive compatibility"),
        "{error:#}"
    );
    let version: i64 = store
        .db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 9);
    drop(claim);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let version: i64 = reopened
        .db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, crate::store::SCHEMA);
}

#[test]
fn simultaneous_old_schema_opens_recheck_migration_under_guard() {
    let (dir, store, _, _) = fixture();
    let path = dir.0.join("lab.db");
    store.db.pragma_update(None, "user_version", 9).unwrap();
    drop(store);
    let barrier = Arc::new(Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let store = Store::open(&path).unwrap();
                let version: i64 = store
                    .db
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(version, crate::store::SCHEMA);
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let db = Connection::open(path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn initialization_probe_preserves_orphans_and_ignores_compatibility_writers() {
    let (dir, store, _, _) = fixture();
    let path = dir.0.join("foreign.db");
    std::fs::write(&path, b"not a database").unwrap();
    let schema = crate::ownership::sidecar(&path, "schema.lock");
    let compat = crate::ownership::sidecar(&path, "lock");
    let old = crate::ownership::lock_file(&compat).unwrap();
    old.try_lock().unwrap();
    for existing_schema in [false, true] {
        if existing_schema {
            std::fs::write(&schema, b"orphan").unwrap();
        }
        let start = std::time::Instant::now();
        crate::wait_for_database_initialization(&path).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(schema.exists(), existing_schema);
        assert_eq!(std::fs::read(&path).unwrap(), b"not a database");
        if existing_schema {
            assert_eq!(std::fs::read(&schema).unwrap(), b"orphan");
        }
    }
    drop(store);
}

#[test]
fn initialization_probe_waits_for_actual_schema_owner() {
    let (dir, _store, _, _) = fixture();
    let path = dir.0.join("lab.db");
    let guard = crate::ownership::schema_guard(&path).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send(crate::wait_for_database_initialization(&path))
            .unwrap();
    });
    assert!(matches!(
        receive.recv_timeout(std::time::Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    receive
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
}

#[test]
fn old_database_wide_execution_lock_excludes_modern_owners_and_vice_versa() {
    let (dir, store, _, _) = fixture();
    let path = dir.0.join("lab.db");
    let old = crate::ownership::lock_file(&crate::ownership::sidecar(&path, "lock")).unwrap();
    assert!(old.try_lock().is_err());
    drop(store);
    old.try_lock().unwrap();
    assert!(Store::open(&path)
        .err()
        .unwrap()
        .to_string()
        .contains("incompatible"));
    old.unlock().unwrap();
    assert!(Store::open(&path).is_ok());
}
