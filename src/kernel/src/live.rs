use crate::model::{OperationId, RunId, ThreadId};
use bob_types::ReasoningKind;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, watch};

const UPDATE_BYTES: usize = 16 * 1024;
const QUEUE_SIZE: usize = 16;
const PREVIEW_BYTES: usize = 256 * 1024;
const REASONING_BLOCKS: usize = 128;
const RETIRED_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelActivity {
    Waiting,
    Thinking,
    Answering,
    ToolCall,
}

#[derive(Clone, Debug)]
pub enum ModelUpdate {
    Text(String),
    Reasoning {
        block_id: String,
        kind: ReasoningKind,
        text: String,
    },
    Activity(ModelActivity),
}

#[derive(Clone, Debug)]
pub struct ReasoningPreview {
    pub block_id: String,
    pub kind: ReasoningKind,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct LiveAttempt {
    pub thread: ThreadId,
    pub run: RunId,
    pub operation: OperationId,
    pub revision: u64,
    pub activity: ModelActivity,
    pub text: String,
    pub reasoning: Vec<ReasoningPreview>,
    pub truncated: bool,
    pub started_at: Instant,
    pub stopping: bool,
    pub settled_sequence: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LiveSnapshot {
    pub instance: String,
    pub revision: u64,
    pub sequence: i64,
    pub attempts: Vec<Arc<LiveAttempt>>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ModelProgress {
    tx: mpsc::Sender<ModelUpdate>,
    accepting: watch::Receiver<bool>,
    truncated: Arc<AtomicBool>,
    sink: Option<Arc<BoundedSink>>,
}

struct BoundedSink {
    bytes: AtomicUsize,
    limit: usize,
}

impl ModelProgress {
    pub(crate) fn bounded_sink(limit: usize) -> (Self, Arc<AtomicBool>) {
        let (tx, _) = mpsc::channel(1);
        let (_, accepting) = watch::channel(false);
        let invalid = Arc::new(AtomicBool::new(false));
        (
            Self {
                tx,
                accepting,
                truncated: invalid.clone(),
                sink: Some(Arc::new(BoundedSink {
                    bytes: AtomicUsize::new(0),
                    limit,
                })),
            },
            invalid,
        )
    }

    pub async fn emit(&self, update: ModelUpdate) -> anyhow::Result<()> {
        if let Some(sink) = &self.sink {
            let bytes = match &update {
                ModelUpdate::Text(text) => text.len(),
                ModelUpdate::Activity(ModelActivity::ToolCall) | ModelUpdate::Reasoning { .. } => {
                    self.truncated.store(true, Ordering::Release);
                    anyhow::bail!("metadata request produced non-text output");
                }
                ModelUpdate::Activity(_) => 0,
            };
            let mut used = sink.bytes.load(Ordering::Acquire);
            loop {
                let Some(total) = used.checked_add(bytes).filter(|total| *total <= sink.limit)
                else {
                    self.truncated.store(true, Ordering::Release);
                    anyhow::bail!("metadata request exceeded its output byte limit");
                };
                match sink.bytes.compare_exchange_weak(
                    used,
                    total,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(()),
                    Err(current) => used = current,
                }
            }
        }
        if !*self.accepting.borrow() {
            return Ok(());
        }
        let (text, block) = match update {
            ModelUpdate::Activity(_) => {
                self.send(update).await;
                return Ok(());
            }
            ModelUpdate::Text(text) => (text, None),
            ModelUpdate::Reasoning {
                block_id,
                kind,
                text,
            } => {
                if block_id.len() > 1024 {
                    self.truncated.store(true, Ordering::Release);
                    anyhow::bail!("reasoning block identifier exceeds 1024 bytes");
                }
                (text, Some((block_id, kind)))
            }
        };
        let budget = UPDATE_BYTES - block.as_ref().map_or(0, |(id, _)| id.len());
        let mut remaining = text.as_str();
        while !remaining.is_empty() {
            let part = prefix(remaining, budget);
            let update = match &block {
                None => ModelUpdate::Text(part.to_owned()),
                Some((block_id, kind)) => ModelUpdate::Reasoning {
                    block_id: block_id.clone(),
                    kind: kind.clone(),
                    text: part.to_owned(),
                },
            };
            if !self.send(update).await {
                break;
            }
            remaining = &remaining[part.len()..];
        }
        Ok(())
    }

    async fn send(&self, update: ModelUpdate) -> bool {
        let mut accepting = self.accepting.clone();
        if !*accepting.borrow_and_update() {
            return false;
        }
        tokio::select! {
            biased;
            _ = accepting.changed() => false,
            permit = self.tx.reserve() => {
                if let Ok(permit) = permit {
                    if *accepting.borrow() {
                        permit.send(update);
                        return true;
                    }
                }
                false
            }
        }
    }
}

struct ActiveAttempt {
    draft: Arc<LiveAttempt>,
    rx: mpsc::Receiver<ModelUpdate>,
    accepting: watch::Sender<bool>,
    truncated: Arc<AtomicBool>,
    bytes: usize,
}

impl ActiveAttempt {
    fn drain(&mut self) -> bool {
        if !*self.accepting.borrow() {
            return false;
        }
        let mut changed = false;
        if self.truncated.swap(false, Ordering::AcqRel) {
            Arc::make_mut(&mut self.draft).truncated = true;
            changed = true;
        }
        // A finite batch keeps a continuously replenished stream from owning the actor.
        for _ in 0..QUEUE_SIZE {
            let Ok(update) = self.rx.try_recv() else {
                break;
            };
            let draft = Arc::make_mut(&mut self.draft);
            let available = PREVIEW_BYTES - self.bytes;
            match update {
                ModelUpdate::Text(text) => {
                    draft.activity = ModelActivity::Answering;
                    let prefix = prefix(&text, available);
                    draft.text.push_str(prefix);
                    self.bytes += prefix.len();
                    draft.truncated |= prefix.len() < text.len();
                }
                ModelUpdate::Reasoning {
                    block_id,
                    kind,
                    text,
                } => {
                    draft.activity = ModelActivity::Thinking;
                    let index = draft
                        .reasoning
                        .iter()
                        .position(|r| r.block_id == block_id && r.kind == kind);
                    let index = match index {
                        Some(index) => index,
                        None if draft.reasoning.len() < REASONING_BLOCKS
                            && block_id.len() <= available =>
                        {
                            self.bytes += block_id.len();
                            draft.reasoning.push(ReasoningPreview {
                                block_id,
                                kind,
                                text: String::new(),
                            });
                            draft.reasoning.len() - 1
                        }
                        None => {
                            draft.truncated = true;
                            changed = true;
                            continue;
                        }
                    };
                    let prefix = prefix(&text, PREVIEW_BYTES - self.bytes);
                    draft.reasoning[index].text.push_str(prefix);
                    self.bytes += prefix.len();
                    draft.truncated |= prefix.len() < text.len();
                }
                ModelUpdate::Activity(activity) => draft.activity = activity,
            }
            changed = true;
        }
        if changed {
            Arc::make_mut(&mut self.draft).revision += 1;
        }
        changed
    }

    fn close(&mut self) {
        self.accepting.send_replace(false);
        self.rx.close();
        let mut discarded = self.truncated.swap(false, Ordering::AcqRel);
        while self.rx.try_recv().is_ok() {
            discarded = true;
        }
        if discarded {
            Arc::make_mut(&mut self.draft).truncated = true;
        }
    }
}

pub(crate) struct LiveProjection {
    tx: watch::Sender<Arc<LiveSnapshot>>,
    active: HashMap<OperationId, ActiveAttempt>,
    retired: VecDeque<Arc<LiveAttempt>>,
    error: Option<String>,
    dirty: bool,
}

impl LiveProjection {
    pub(crate) fn new(sequence: i64) -> Self {
        let (tx, _) = watch::channel(Arc::new(LiveSnapshot {
            instance: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            sequence,
            attempts: Vec::new(),
            error: None,
        }));
        Self {
            tx,
            active: HashMap::new(),
            retired: VecDeque::new(),
            error: None,
            dirty: false,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<LiveSnapshot>> {
        self.tx.subscribe()
    }

    pub(crate) fn start(
        &mut self,
        thread: ThreadId,
        run: RunId,
        operation: OperationId,
    ) -> ModelProgress {
        assert!(self.active.len() < 16 && !self.active.contains_key(&operation));
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let (accepting, signal) = watch::channel(true);
        let truncated = Arc::new(AtomicBool::new(false));
        self.active.insert(
            operation.clone(),
            ActiveAttempt {
                draft: Arc::new(LiveAttempt {
                    thread,
                    run,
                    operation,
                    revision: 1,
                    activity: ModelActivity::Waiting,
                    text: String::new(),
                    reasoning: Vec::new(),
                    truncated: false,
                    started_at: Instant::now(),
                    stopping: false,
                    settled_sequence: None,
                    error: None,
                }),
                rx,
                accepting,
                truncated: truncated.clone(),
                bytes: 0,
            },
        );
        self.dirty = true;
        ModelProgress {
            tx,
            accepting: signal,
            truncated,
            sink: None,
        }
    }

    pub(crate) fn collect(&mut self) {
        for attempt in self.active.values_mut() {
            self.dirty |= attempt.drain();
        }
    }

    pub(crate) fn stop(&mut self, operation: &OperationId) {
        if let Some(attempt) = self.active.get_mut(operation) {
            if !attempt.draft.stopping {
                attempt.close();
                let draft = Arc::make_mut(&mut attempt.draft);
                draft.stopping = true;
                draft.revision += 1;
                self.dirty = true;
            }
        }
    }

    pub(crate) fn settle(&mut self, operation: &OperationId, sequence: i64, error: Option<String>) {
        if let Some(mut attempt) = self.active.remove(operation) {
            attempt.drain();
            attempt.close();
            let draft = Arc::make_mut(&mut attempt.draft);
            draft.settled_sequence = Some(sequence);
            draft.error = error.map(|e| prefix(&e, 2048).to_owned());
            draft.revision += 1;
            self.retired.push_back(attempt.draft);
            if self.retired.len() > RETIRED_ATTEMPTS {
                self.retired.pop_front();
            }
            self.dirty = true;
        }
    }

    pub(crate) fn fail(&mut self, error: &str) {
        let error = prefix(error, 2048);
        if self.error.as_deref() != Some(error) {
            self.error = Some(error.to_owned());
            self.dirty = true;
        }
        for attempt in self.active.values_mut() {
            if attempt.draft.error.is_none() {
                attempt.close();
                let draft = Arc::make_mut(&mut attempt.draft);
                draft.error = Some(prefix(error, 2048).to_owned());
                draft.stopping = true;
                draft.revision += 1;
                self.dirty = true;
            }
        }
        let sequence = self.tx.borrow().sequence;
        self.publish(sequence);
    }

    pub(crate) fn publish(&mut self, sequence: i64) {
        let current = self.tx.borrow().clone();
        if !self.dirty && current.sequence == sequence {
            return;
        }
        let mut attempts: Vec<_> = self
            .retired
            .iter()
            .cloned()
            .chain(self.active.values().map(|a| a.draft.clone()))
            .collect();
        attempts.sort_by_key(|a| a.started_at);
        self.tx.send_replace(Arc::new(LiveSnapshot {
            instance: current.instance.clone(),
            revision: current.revision + 1,
            sequence,
            attempts,
            error: self.error.clone(),
        }));
        self.dirty = false;
    }
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_sink_shares_its_byte_limit_across_concurrent_emitters() {
        let (sink, invalid) = ModelProgress::bounded_sink(21);
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let sink = sink.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                barrier.wait().await;
                sink.emit(ModelUpdate::Text("界".into())).await.is_ok()
            });
        }
        let mut accepted = 0;
        while let Some(result) = tasks.join_next().await {
            accepted += usize::from(result.unwrap());
        }
        assert_eq!(accepted, 7);
        assert_eq!(
            sink.sink.as_ref().unwrap().bytes.load(Ordering::Acquire),
            21
        );
        assert!(invalid.load(Ordering::Acquire));
    }

    #[test]
    fn global_failure_publishes_without_active_models_or_sequence_advance() {
        let mut live = LiveProjection::new(17);
        let mut watch = live.subscribe();
        let error = "界".repeat(1024);
        live.fail(&error);
        assert!(watch.has_changed().unwrap());
        let failed = watch.borrow_and_update().clone();
        assert_eq!(failed.sequence, 17);
        assert_eq!(failed.revision, 1);
        assert!(failed.attempts.is_empty());
        assert_eq!(failed.error.as_deref(), Some("界".repeat(682).as_str()));
        live.fail(&error);
        assert!(!watch.has_changed().unwrap());
        live.publish(18);
        assert_eq!(watch.borrow().error, failed.error);
    }
}
