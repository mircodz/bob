//! Token-usage accounting across three layers that share one record type:
//!   * per-message  — one `UsageEntry` per provider completion
//!   * per-session  — a session owns a `Vec<UsageEntry>` (summed on demand)
//!   * global       — every entry is also appended to ~/.bob/usage.jsonl, the
//!                    append-only ledger the future /usage dashboard reads.

use crate::core::types::Usage;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// One accounted unit of work: the tokens a single provider completion cost,
/// tagged with enough context to slice it later (by session, model, day, agent).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageEntry {
    /// Unix seconds when the entry was recorded.
    pub ts: u64,
    /// The session this belongs to.
    pub session_id: String,
    /// Provider name (e.g. "anthropic", "openai").
    pub provider: String,
    /// Model that produced the completion.
    pub model: String,
    /// "root" or a subagent id like "task_1".
    pub agent_id: String,
    /// The token counts for this completion.
    pub usage: Usage,
}

/// Sum a slice of entries into a single Usage total.
pub fn total_of(entries: &[UsageEntry]) -> Usage {
    let mut acc = Usage::default();
    for e in entries {
        acc.add(&e.usage);
    }
    acc
}

fn ledger_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".bob").join("usage.jsonl")
}

/// Append one entry to the global ledger (one JSON object per line). Best-effort:
/// errors are returned but callers typically ignore them (accounting must never
/// break the agent loop).
pub fn append_global(entry: &UsageEntry) -> anyhow::Result<()> {
    let path = ledger_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", line)?;
    Ok(())
}

/// Read the entire global ledger (for totals / the dashboard). Malformed lines
/// are skipped rather than failing the whole read.
pub fn read_global() -> anyhow::Result<Vec<UsageEntry>> {
    let path = ledger_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<UsageEntry>(l).ok())
        .collect())
}

/// Grand total across all sessions ever recorded.
pub fn global_total() -> Usage {
    read_global().map(|e| total_of(&e)).unwrap_or_default()
}
