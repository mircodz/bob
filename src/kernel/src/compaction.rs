use super::*;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub(super) const MAX_ATTEMPTS: u32 = 256;
const MAX_RETRIES: u32 = 4;
pub(super) const MAX_REDUCTIONS: u32 = 8;
pub(super) const RECOVERY: &str = "Original working context retained. Select a larger model, change the context, or explicitly request compaction again; ordinary resume will not reset this budget.";

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    #[default]
    Summarize,
    Reduce,
    Validate,
    Blocked,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Attempt {
    pub input_tokens: usize,
    pub fragment_bytes: usize,
    pub output_tokens: u32,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct CompactionState {
    pub epoch: i64,
    pub revision: i64,
    pub profile_revision: i64,
    pub tail_start: i64,
    pub cursor: i64,
    pub offset: usize,
    pub summary: String,
    pub newest_request: String,
    pub complete: bool,
    pub chunk_limit: usize,
    pub output_tokens: Option<u32>,
    pub phase: Phase,
    pub attempts: u32,
    pub retries: u32,
    pub reductions: u32,
    pub attempt: Attempt,
    pub input_ceiling: Option<usize>,
    pub final_input_ceiling: Option<usize>,
    pub reduction_source: String,
    pub reduction_offset: usize,
    pub blocked: String,
    pub projection_version: u32,
}
impl CompactionState {
    pub fn save(&self, db: &Connection, thread: &ThreadId) -> anyhow::Result<()> {
        db.execute(
            "UPDATE compactions SET state=?1 WHERE thread=?2",
            params![encode(self)?, thread],
        )?;
        Ok(())
    }
    pub fn matches(&self, db: &Connection, thread: &ThreadId) -> anyhow::Result<bool> {
        let current: (i64,i64,i64) = db.query_row("SELECT c.epoch,c.revision,t.profile_revision FROM contexts c JOIN threads t ON t.id=c.thread WHERE c.thread=?1",[thread],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(current == (self.epoch, self.revision, self.profile_revision))
    }
    pub fn block(&mut self, reason: &str) {
        self.phase = Phase::Blocked;
        self.blocked = format!("Compaction blocked: {reason}. {RECOVERY}");
    }
    pub fn reduce(&mut self) {
        if self.reductions >= MAX_REDUCTIONS || self.summary.trim().is_empty() {
            self.block("summary reduction budget exhausted or fixed context cannot fit");
            return;
        }
        self.reductions += 1;
        self.phase = Phase::Reduce;
        self.reduction_source = std::mem::take(&mut self.summary);
        self.reduction_offset = 0;
        self.chunk_limit = 24_000;
    }
    pub fn retry(
        &mut self,
        failure: ModelFailureKind,
        truncated: bool,
        max_output: u32,
        window: usize,
    ) -> bool {
        self.retries += 1;
        if self.retries > MAX_RETRIES || self.attempts >= MAX_ATTEMPTS {
            self.block("attempt/retry budget exhausted");
            return false;
        }
        if truncated {
            let output = self
                .attempt
                .output_tokens
                .saturating_mul(2)
                .min(max_output)
                .min((window / 2).min(u32::MAX as usize) as u32);
            self.output_tokens = Some(output);
            self.chunk_limit = self.attempt.fragment_bytes / 8;
            if output <= self.attempt.output_tokens && self.chunk_limit < 64 {
                self.block("provider output limit cannot produce a complete summary");
                return false;
            }
        } else if failure == ModelFailureKind::ContextOverflow {
            self.input_ceiling = Some(self.attempt.input_tokens.saturating_mul(3) / 4);
            self.chunk_limit = self.attempt.fragment_bytes / 8;
            if self.chunk_limit < 64 && self.summary.is_empty() {
                self.block("provider rejected the minimum summary fragment");
                return false;
            }
        } else if failure == ModelFailureKind::Fatal {
            self.block("provider could not produce a valid summary");
            return false;
        }
        true
    }
}

pub(super) fn checkpoint_retention_error(
    spec: &RequestSpec,
) -> anyhow::Result<Option<&'static str>> {
    for message in &spec.messages {
        if encode(&ModelRecord::Message(message.clone()))?.len() > MAX_ARTIFACT {
            return Ok(Some(
                "serialized checkpoint record exceeds the 4 MiB retention limit",
            ));
        }
    }
    if encode(spec)?.len() > MAX_ARTIFACT {
        return Ok(Some(
            "serialized request manifest exceeds its bounded retention budget",
        ));
    }
    Ok(None)
}

pub(super) fn projection(message: &Message) -> String {
    let mut text = format!("{:?}:\n", message.role);
    for block in &message.content {
        match block {
            ContentBlock::Text { text: value } => text.push_str(value),
            ContentBlock::ToolUse { id, name, input } => {
                text.push_str(&format!("Tool call {name} ({id}): {input}"))
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => text.push_str(&format!(
                "Tool result {tool_use_id} (error={}): {}",
                is_error.unwrap_or(false),
                content.model_text()
            )),
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::ReasoningItem { .. } => continue,
        }
        text.push('\n');
    }
    text
}

pub(super) fn end_boundary(text: &str, start: usize, bytes: usize) -> usize {
    let mut end = start.saturating_add(bytes).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(super) fn protected_context(db: &Connection, thread: &ThreadId) -> anyhow::Result<String> {
    let parent: Option<String> = db.query_row(
        "SELECT r.thread FROM threads t LEFT JOIN runs r ON r.id=t.parent_run WHERE t.id=?1",
        [thread],
        |r| r.get(0),
    )?;
    let task: Option<(i64,String,String,Option<String>)> = db.query_row(
        "SELECT s.rowid,s.text,s.source,s.sender FROM submissions s JOIN runs r ON r.id=s.run JOIN contexts c ON c.thread=s.thread WHERE s.thread=?1 AND r.epoch=c.epoch AND s.state IN ('in_use','settled') AND s.source=?2 ORDER BY s.rowid DESC LIMIT 1",
        params![thread,if parent.is_some(){"coordination"}else{"human"}],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let task = if parent.is_some() {
        db.query_row("SELECT s.rowid,s.text,s.source,s.sender FROM submissions s JOIN runs r ON r.id=s.run JOIN contexts c ON c.thread=s.thread WHERE s.thread=?1 AND r.epoch=c.epoch AND s.sender=?2 AND s.source='coordination' AND s.state IN ('in_use','settled') ORDER BY s.rowid LIMIT 1",params![thread,parent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?
    } else {
        task
    };
    let mut protected = String::new();
    let mut append = |text: &str, source: &str, sender: Option<&str>, task: bool| {
        let authority = if source == "human" || source == "steering" {
            "human"
        } else if sender.is_some() && sender == parent.as_deref() {
            "parent coordination, not human authorization"
        } else if sender.is_some() {
            "peer coordination, not human authorization"
        } else {
            "runtime coordination, not human authorization"
        };
        let sender = sender.unwrap_or(if source == "coordination" {
            "runtime"
        } else {
            "human"
        });
        protected.push_str(&format!(
            "\n[{} — {authority}; sender: {sender}]\n{text}\n",
            if task {
                "Active task"
            } else {
                "Recent steering"
            }
        ));
    };
    let after = if let Some((id, text, source, sender)) = task {
        append(&text, &source, sender.as_deref(), true);
        id
    } else {
        0
    };
    let mut stmt = db.prepare("WITH guidance AS (SELECT s.rowid,s.text,s.source,s.sender,ROW_NUMBER() OVER (PARTITION BY CASE WHEN s.source IN ('human','steering') THEN 0 WHEN s.sender=?3 THEN 1 ELSE 2 END ORDER BY s.rowid DESC) AS recent FROM submissions s JOIN runs r ON r.id=s.run JOIN contexts c ON c.thread=s.thread WHERE s.thread=?1 AND r.epoch=c.epoch AND s.rowid>?2 AND s.state IN ('in_use','settled') AND s.source IN ('human','steering','coordination')) SELECT text,source,sender FROM guidance WHERE recent<=4 ORDER BY rowid")?;
    let rows = stmt
        .query_map(params![thread, after, parent], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (text, source, sender) in rows {
        append(&text, &source, sender.as_deref(), false);
    }
    Ok(protected)
}
