use crate::{ArtifactId, TextPage, ThreadId};
use anyhow::Context;
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

enum Input {
    Entry(
        ThreadId,
        i64,
        usize,
        usize,
        oneshot::Sender<anyhow::Result<TextPage>>,
    ),
    Close(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct Reader {
    tx: mpsc::Sender<Input>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl Reader {
    pub(crate) async fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_owned();
        let (tx, mut rx) = mpsc::channel::<Input>(16);
        let (ready, initialized) = oneshot::channel();
        let thread = std::thread::Builder::new().name("bob-history-reader".into()).spawn(move || {
            let db = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
                Ok(db) => db,
                Err(error) => { let _ = ready.send(Err(anyhow::Error::from(error))); return; }
            };
            let store = crate::store::Store::read_only(db);
            let mut cache = crate::visible_text::Cache::default();
            let _ = ready.send(Ok(()));
            while let Some(input) = rx.blocking_recv() {
                match input {
                    Input::Entry(thread, sequence, offset, limit, reply) => {
                        if reply.is_closed() { continue; }
                        let result = (|| {
                            let source: Option<String> = store.db.query_row(
                                "SELECT CASE WHEN kind='assistant' THEN COALESCE(model,result) END FROM entries WHERE thread=?1 AND seq=?2",
                                params![thread, sequence], |r| r.get(0))?;
                            if let Some(source) = source {
                                cache.page(&store.db, &ArtifactId(source), offset, limit)
                            } else {
                                store.entry_text(&thread, sequence, offset, limit)
                            }
                        })();
                        let _ = reply.send(result);
                    }
                    Input::Close(reply) => { let _ = reply.send(()); break; }
                }
            }
        })?;
        initialized
            .await
            .context("history reader initialization stopped")??;
        Ok(Self {
            tx,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    pub(crate) async fn entry_text(
        &self,
        thread: ThreadId,
        sequence: i64,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Input::Entry(
                thread,
                sequence,
                offset,
                limit.clamp(4, 65536),
                tx,
            ))
            .await?;
        rx.await?
    }

    pub(crate) async fn close(&self) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Input::Close(tx)).await.is_ok() {
            let _ = rx.await;
        }
        let thread = self
            .thread
            .lock()
            .map_err(|_| anyhow::anyhow!("history reader join lock poisoned"))?
            .take();
        if let Some(thread) = thread {
            tokio::task::spawn_blocking(move || {
                thread
                    .join()
                    .map_err(|_| anyhow::anyhow!("history reader panicked"))
            })
            .await??;
        }
        Ok(())
    }
}
