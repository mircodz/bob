use super::*;
use std::cell::Cell;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("bob-worker-contention-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn terminal() -> WorkerFrame {
    WorkerFrame::Final {
        outcome: ToolOutcome {
            text: "healthy authoritative capture".into(),
            is_error: false,
            interrupted: false,
            presentation: None,
        },
    }
}

#[tokio::test(start_paused = true)]
async fn full_protocol_allowance_drains_while_sqlite_append_waits_past_collection_deadline() {
    let dir = TestDir::new();
    let path = dir.0.join("progress.sqlite");
    let sink = rusqlite::Connection::open(&path).unwrap();
    sink.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE progress(text TEXT NOT NULL)")
        .unwrap();
    sink.busy_timeout(Duration::ZERO).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (source, mut writer) = tokio::io::duplex(1024);
    let produced = Cell::new(false);
    let calls = Cell::new(0usize);
    let in_flight = Cell::new(false);
    let producer = async {
        for index in 0..PROGRESS_FRAME_LIMIT + 2 {
            let text = if index < PROGRESS_FRAME_LIMIT {
                format!("{index:04}{}", "x".repeat(4092))
            } else {
                String::new()
            };
            write_frame(
                &mut writer,
                &WorkerFrame::Progress {
                    stream: if index % 2 == 0 {
                        WireStream::Stdout
                    } else {
                        WireStream::Stderr
                    },
                    text,
                },
            )
            .await
            .unwrap();
        }
        write_frame(&mut writer, &terminal()).await.unwrap();
        writer.shutdown().await.unwrap();
        produced.set(true);
    };
    let release = async {
        tokio::time::sleep(PROGRESS_WRITE_TIMEOUT + Duration::from_secs(1)).await;
        assert!(
            produced.get(),
            "helper output blocked behind durable forwarding"
        );
        assert_eq!(calls.get(), 1, "more than one append was in flight");
        assert!(in_flight.get());
        blocker.execute_batch("ROLLBACK").unwrap();
    };
    let mut append = |text: String| {
        assert!(!in_flight.replace(true));
        calls.set(calls.get() + 1);
        let sink = &sink;
        let in_flight = &in_flight;
        async move {
            loop {
                match sink.execute("INSERT INTO progress(text) VALUES(?1)", [&text]) {
                    Ok(_) => break,
                    Err(rusqlite::Error::SqliteFailure(code, _))
                        if code.code == rusqlite::ErrorCode::DatabaseBusy =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            in_flight.set(false);
            Ok(())
        }
    };
    let mut reader = ProtocolReader::new(Some(source));
    let consumer = reader.collect_with(Some(PROGRESS_WRITE_TIMEOUT), async { Ok(()) }, &mut append);
    let ((), (), result) = tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(producer, release, consumer)
    })
    .await
    .unwrap();
    result.unwrap();
    assert_eq!(reader.progress_frames, PROGRESS_FRAME_LIMIT + 2);
    assert_eq!(reader.progress_bytes, PROGRESS_TEXT_LIMIT);
    assert_eq!(calls.get(), PROGRESS_FRAME_LIMIT + 2);
    let rows = sink
        .prepare("SELECT text FROM progress ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), PROGRESS_FRAME_LIMIT + 2);
    for (index, row) in rows.iter().enumerate() {
        let label = if index % 2 == 0 {
            "stdout:\n"
        } else {
            "stderr:\n"
        };
        let expected = if index < PROGRESS_FRAME_LIMIT {
            format!("{label}{index:04}{}", "x".repeat(4092))
        } else {
            label.to_owned()
        };
        assert_eq!(row, &expected);
    }
    let outcome = reader.take_outcome().unwrap();
    assert_eq!(outcome.text, "healthy authoritative capture");
    assert!(!outcome.interrupted && !outcome.is_error);
    assert!(outcome.presentation.is_none());
}

#[tokio::test(start_paused = true)]
async fn transport_deadline_still_bounds_a_missing_terminal_frame() {
    let (source, _writer) = tokio::io::duplex(64);
    let mut reader = ProtocolReader::new(Some(source));
    let error = reader
        .collect_with(
            Some(Duration::from_millis(50)),
            async { Ok(()) },
            &mut |_| std::future::ready(Ok(())),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cleanup deadline"));
    assert!(reader.outcome.is_none());
}

#[tokio::test(start_paused = true)]
async fn cancelled_append_is_not_replayed_after_terminal_was_already_drained() {
    let mut wire = Vec::new();
    for text in ["first", "second", "third"] {
        write_frame(
            &mut wire,
            &WorkerFrame::Progress {
                stream: WireStream::Stdout,
                text: text.into(),
            },
        )
        .await
        .unwrap();
    }
    write_frame(&mut wire, &terminal()).await.unwrap();
    let mut reader = ProtocolReader::new(Some(wire.as_slice()));
    let mut attempts = 0;
    assert!(tokio::time::timeout(
        Duration::from_millis(50),
        reader.drain_with(&mut |_| {
            attempts += 1;
            std::future::pending::<Result<()>>()
        })
    )
    .await
    .is_err());
    assert_eq!(attempts, 1);
    assert!(
        reader.outcome.is_some(),
        "terminal collection waited for append acknowledgment"
    );
    reader
        .drain_with(&mut |_| -> std::future::Ready<Result<()>> {
            panic!("uncertain append or queued tail was replayed")
        })
        .await
        .unwrap();
    let outcome = reader.take_outcome().unwrap();
    assert_eq!(outcome.text, "healthy authoritative capture");
    assert!(!outcome.interrupted);
    assert!(outcome
        .presentation
        .unwrap()
        .summary
        .unwrap()
        .contains("progress lost"));
}
