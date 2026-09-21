//! Per-completion token accounting for sessions and the `~/.bob/usage.jsonl` ledger.

use crate::core::types::Usage;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageEntry {
    /// Unix seconds when the entry was recorded.
    pub ts: u64,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    /// "root" or a subagent id like "task_1".
    pub agent_id: String,
    pub usage: Usage,
}

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

/// Append one JSON object per line. Callers may ignore errors so accounting
/// failures do not interrupt the agent loop.
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

/// Malformed ledger lines are skipped rather than failing the whole read.
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

pub fn global_total() -> Usage {
    read_global().map(|e| total_of(&e)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ledger_totals_include_all_counters_and_legacy_entries() {
        let entries: Vec<UsageEntry> = serde_json::from_value(json!([
            {"ts": 1, "session_id": "one", "provider": "test", "model": "test", "agent_id": "root",
             "usage": {"input_tokens": 2, "output_tokens": 3}},
            {"ts": 2, "session_id": "two", "provider": "test", "model": "test", "agent_id": "child",
             "usage": {"input_tokens": 5, "output_tokens": 7, "cache_creation_input_tokens": 11, "cache_read_input_tokens": 13}}
        ])).unwrap();
        assert_eq!(
            serde_json::to_value(total_of(&entries)).unwrap(),
            json!({
                "input_tokens": 7, "output_tokens": 10, "cache_creation_input_tokens": 11, "cache_read_input_tokens": 13,
            })
        );
        assert_eq!(total_of(&entries).total_input(), 31);
        assert_eq!(
            serde_json::to_value(total_of(&[])).unwrap(),
            json!({
                "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            })
        );
    }

    fn entry(session: &str, model: &str, agent: &str, counters: [u64; 4]) -> UsageEntry {
        UsageEntry {
            ts: 123,
            session_id: session.into(),
            provider: "test-provider".into(),
            model: model.into(),
            agent_id: agent.into(),
            usage: Usage {
                input_tokens: counters[0],
                output_tokens: counters[1],
                cache_creation_input_tokens: counters[2],
                cache_read_input_tokens: counters[3],
            },
        }
    }

    #[test]
    fn totals_count_each_completion_without_deduplicating_or_filtering_metadata() {
        let first = entry("session-a", "model-a", "root", [2, 3, 5, 7]);
        let entries = vec![
            first.clone(),
            first,
            entry("session-a", "model-b", "child", [11, 13, 17, 19]),
            entry("session-b", "model-a", "root", [23, 29, 31, 37]),
            entry("session-c", "model-c", "child", [0, 0, 0, 0]),
        ];
        let expected = json!({
            "input_tokens": 38,
            "output_tokens": 48,
            "cache_creation_input_tokens": 58,
            "cache_read_input_tokens": 70,
        });
        assert_eq!(serde_json::to_value(total_of(&entries)).unwrap(), expected);
    }

    #[test]
    fn totals_keep_cache_only_completions_and_large_counters_separate() {
        let entries = [
            entry("a", "m", "root", [4_294_967_296, 0, 0, 0]),
            entry("a", "m", "child", [0, 4_294_967_297, 0, 0]),
            entry("b", "m", "root", [0, 0, 4_294_967_298, 0]),
            entry("b", "m", "child", [0, 0, 0, 4_294_967_299]),
        ];
        assert_eq!(
            serde_json::to_value(total_of(&entries)).unwrap(),
            json!({
                "input_tokens": 4_294_967_296_u64,
                "output_tokens": 4_294_967_297_u64,
                "cache_creation_input_tokens": 4_294_967_298_u64,
                "cache_read_input_tokens": 4_294_967_299_u64,
            })
        );
        assert_eq!(
            serde_json::to_value(total_of(&entries[2..])).unwrap(),
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 4_294_967_298_u64,
                "cache_read_input_tokens": 4_294_967_299_u64,
            })
        );
    }

    #[test]
    fn usage_entry_wire_format_preserves_accounting_dimensions_and_legacy_cache_defaults() {
        let legacy = json!({
            "ts": 1700000000,
            "session_id": "session-1",
            "provider": "provider-a",
            "model": "model-b",
            "agent_id": "task-2",
            "usage": {"input_tokens": 13, "output_tokens": 17},
            "future_field": "ignored",
        });
        let decoded: UsageEntry = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            json!({
                "ts": 1700000000,
                "session_id": "session-1",
                "provider": "provider-a",
                "model": "model-b",
                "agent_id": "task-2",
                "usage": {
                    "input_tokens": 13,
                    "output_tokens": 17,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            })
        );
        let serialized = serde_json::to_value(entry("s", "m", "root", [2, 3, 5, 7])).unwrap();
        assert_eq!(
            serialized,
            json!({
                "ts": 123,
                "session_id": "s",
                "provider": "test-provider",
                "model": "m",
                "agent_id": "root",
                "usage": {"input_tokens": 2, "output_tokens": 3, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 7},
            })
        );
    }
}
