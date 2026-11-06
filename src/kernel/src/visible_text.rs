use crate::model::{ArtifactId, TextPage};
use crate::store::MAX_ARTIFACT;
use anyhow::{ensure, Context};
use bob_types::{ContentBlock, Message, Role};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::{Error, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::VecDeque;
use std::fmt;

const CHUNK: usize = 64 * 1024;
const VERSION: i64 = 1;
const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_ENTRIES: usize = 32;

#[derive(Debug)]
pub(crate) struct BoundaryError;

impl fmt::Display for BoundaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("entry offset must be a retained UTF-8 boundary")
    }
}

impl std::error::Error for BoundaryError {}
// One source, serde scratch/one text block, growing output, and a bounded chunk.
const EXTRACTION_RESERVATION: usize = 8 * MAX_ARTIFACT + CHUNK;

pub(crate) const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS assistant_visible_text(
        source TEXT PRIMARY KEY REFERENCES artifacts(id),
        version INTEGER NOT NULL CHECK(version=1),
        artifact TEXT NOT NULL UNIQUE REFERENCES artifacts(id),
        total_bytes INTEGER NOT NULL CHECK(total_bytes BETWEEN 0 AND 4194304)
    ) WITHOUT ROWID;
    CREATE TRIGGER IF NOT EXISTS assistant_visible_text_immutable
    BEFORE UPDATE ON assistant_visible_text BEGIN
        SELECT RAISE(ABORT,'assistant visible text is immutable');
    END;
    CREATE TRIGGER IF NOT EXISTS assistant_visible_text_no_delete
    BEFORE DELETE ON assistant_visible_text BEGIN
        SELECT RAISE(ABORT,'assistant visible text is immutable');
    END;
";

fn artifact_length(db: &Connection, artifact: &ArtifactId) -> anyhow::Result<usize> {
    let length: i64 = db
        .query_row(
            "SELECT length FROM artifacts WHERE id=?1",
            [artifact],
            |r| r.get(0),
        )
        .optional()?
        .context("visible text artifact is missing")?;
    ensure!(
        (0..=MAX_ARTIFACT as i64).contains(&length),
        "visible text artifact exceeds retention bounds"
    );
    Ok(length as usize)
}

fn projection(db: &Connection, source: &ArtifactId) -> anyhow::Result<Option<(ArtifactId, usize)>> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='assistant_visible_text')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let row: Option<(i64, ArtifactId, i64)> = db
        .query_row(
            "SELECT version,artifact,total_bytes FROM assistant_visible_text WHERE source=?1",
            [source],
            |r| Ok((r.get(0)?, ArtifactId(r.get(1)?), r.get(2)?)),
        )
        .optional()?;
    let Some((version, artifact, length)) = row else {
        return Ok(None);
    };
    ensure!(
        version == VERSION,
        "unsupported assistant visible text version {version}"
    );
    ensure!(
        (0..=MAX_ARTIFACT as i64).contains(&length),
        "visible text length exceeds retention bounds"
    );
    ensure!(
        artifact_length(db, &artifact)? == length as usize,
        "visible text artifact length mismatch"
    );
    Ok(Some((artifact, length as usize)))
}

pub(crate) fn retain(
    db: &Connection,
    source: &ArtifactId,
    message: &Message,
) -> anyhow::Result<ArtifactId> {
    ensure!(
        !db.is_autocommit(),
        "visible text retention requires the message settlement transaction"
    );
    ensure!(
        message.role == Role::Assistant,
        "visible text source is not an assistant message"
    );
    artifact_length(db, source)?;
    ensure!(
        projection(db, source)?.is_none(),
        "assistant visible text already retained"
    );
    let total = message.content.iter().try_fold(0usize, |total, block| {
        let length = match block {
            ContentBlock::Text { text } => text.len(),
            _ => 0,
        };
        let total = total
            .checked_add(length)
            .context("visible text length overflow")?;
        ensure!(
            total <= MAX_ARTIFACT,
            "visible text exceeds retention bounds"
        );
        Ok::<_, anyhow::Error>(total)
    })?;
    let artifact = ArtifactId::new();
    db.execute(
        "INSERT INTO artifacts(id,length) VALUES(?1,?2)",
        params![artifact, total],
    )?;
    let mut buffer = Vec::with_capacity(CHUNK.min(total));
    let mut ordinal = 0;
    for block in &message.content {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            let count = bytes.len().min(CHUNK - buffer.len());
            buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if buffer.len() == CHUNK {
                db.execute(
                    "INSERT INTO chunks(artifact,ordinal,data) VALUES(?1,?2,?3)",
                    params![artifact, ordinal, buffer],
                )?;
                buffer.clear();
                ordinal += 1;
            }
        }
    }
    if !buffer.is_empty() {
        db.execute(
            "INSERT INTO chunks(artifact,ordinal,data) VALUES(?1,?2,?3)",
            params![artifact, ordinal, buffer],
        )?;
    }
    db.execute(
        "INSERT INTO assistant_visible_text(source,version,artifact,total_bytes) VALUES(?1,?2,?3,?4)",
        params![source, VERSION, artifact, total],
    )?;
    Ok(artifact)
}

#[cfg(test)]
fn is_preview(db: &Connection, source: &ArtifactId, preview: &str) -> anyhow::Result<Option<bool>> {
    projection(db, source)?
        .map(|(_, length)| {
            ensure!(
                preview.len() <= length,
                "assistant preview exceeds visible text length"
            );
            Ok(preview.len() < length)
        })
        .transpose()
}

fn read_range(
    db: &Connection,
    artifact: &ArtifactId,
    length: usize,
    start: usize,
    end: usize,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        start <= end && end <= length && length <= MAX_ARTIFACT,
        "invalid visible text byte range"
    );
    if start == end {
        return Ok(Vec::new());
    }
    let first = start / CHUNK;
    let last = (end - 1) / CHUNK;
    let mut statement = db.prepare(
        "SELECT ordinal,length(data),typeof(data),CASE WHEN typeof(data)='blob' AND length(data)<=65536 THEN data END
         FROM chunks WHERE artifact=?1 AND ordinal BETWEEN ?2 AND ?3 ORDER BY ordinal",
    )?;
    let mut rows = statement.query(params![artifact, first, last])?;
    let mut bytes = Vec::with_capacity(end - start);
    for ordinal in first..=last {
        let row = rows
            .next()?
            .context("visible text artifact chunk is missing")?;
        let actual: i64 = row.get(0)?;
        let size: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let chunk_start = ordinal * CHUNK;
        let expected = CHUNK.min(length - chunk_start);
        ensure!(
            actual == ordinal as i64,
            "visible text artifact chunk is missing"
        );
        ensure!(
            kind == "blob" && size == expected as i64,
            "visible text artifact chunk has invalid length or type"
        );
        let blob = row
            .get_ref(3)?
            .as_blob()
            .context("visible text artifact chunk is corrupt")?;
        let from = start.saturating_sub(chunk_start);
        let to = (end - chunk_start).min(expected);
        bytes.extend_from_slice(&blob[from..to]);
    }
    ensure!(
        bytes.len() == end - start,
        "visible text artifact range is incomplete"
    );
    Ok(bytes)
}

fn text_page(text: &str, offset: usize, limit: usize) -> anyhow::Result<TextPage> {
    ensure!(
        offset <= text.len(),
        "entry offset exceeds retained visible text length"
    );
    if !text.is_char_boundary(offset) {
        return Err(BoundaryError.into());
    }
    let mut end = offset.saturating_add(limit.clamp(4, CHUNK)).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(TextPage {
        text: text[offset..end].to_owned(),
        next_offset: (end < text.len()).then_some(end),
        total_bytes: text.len(),
    })
}

pub(crate) fn page(
    db: &Connection,
    source: &ArtifactId,
    offset: usize,
    limit: usize,
) -> anyhow::Result<Option<TextPage>> {
    let Some((artifact, length)) = projection(db, source)? else {
        return Ok(None);
    };
    ensure!(
        offset <= length,
        "entry offset exceeds retained visible text length"
    );
    let end = offset.saturating_add(limit.clamp(4, CHUNK)).min(length);
    let mut bytes = read_range(db, &artifact, length, offset, end)?;
    if let Err(error) = std::str::from_utf8(&bytes) {
        if bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
            let start = offset.saturating_sub(3);
            let probe = read_range(db, &artifact, length, start, (offset + 4).min(length))?;
            let position = offset - start;
            for previous in 0..position {
                for width in 2..=4 {
                    if previous + width > position
                        && previous + width <= probe.len()
                        && std::str::from_utf8(&probe[previous..previous + width])
                            .is_ok_and(|text| text.chars().count() == 1)
                    {
                        return Err(BoundaryError.into());
                    }
                }
            }
        }
        ensure!(
            error.error_len().is_none() && end < length && error.valid_up_to() > 0,
            "visible text artifact is not UTF-8"
        );
        bytes.truncate(error.valid_up_to());
    }
    let text = String::from_utf8(bytes).context("visible text artifact is not UTF-8")?;
    let end = offset + text.len();
    Ok(Some(TextPage {
        text,
        next_offset: (end < length).then_some(end),
        total_bytes: length,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlockKind {
    Text,
    ToolUse,
    ToolResult,
    Thinking,
    RedactedThinking,
    ReasoningItem,
}

#[derive(Deserialize)]
struct VisibleBlock {
    #[serde(rename = "type")]
    kind: BlockKind,
    text: Option<String>,
}

#[derive(Deserialize)]
struct VisibleMessage {
    role: Role,
    #[serde(deserialize_with = "visible_content")]
    content: String,
}

fn visible_content<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct TextVisitor;
    impl<'de> Visitor<'de> for TextVisitor {
        type Value = String;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("message content blocks")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<String, A::Error> {
            let mut output = String::new();
            while let Some(block) = sequence.next_element::<VisibleBlock>()? {
                if matches!(block.kind, BlockKind::Text) {
                    let text = block
                        .text
                        .ok_or_else(|| A::Error::custom("text block has no text"))?;
                    let length = output
                        .len()
                        .checked_add(text.len())
                        .ok_or_else(|| A::Error::custom("visible text length overflow"))?;
                    if length > MAX_ARTIFACT {
                        return Err(A::Error::custom("visible text exceeds retention bounds"));
                    }
                    if length > output.capacity() {
                        let capacity = length
                            .max(output.capacity().saturating_mul(2))
                            .min(MAX_ARTIFACT);
                        output.reserve_exact(capacity - output.len());
                    }
                    output.push_str(&text);
                }
            }
            Ok(output)
        }
    }
    deserializer.deserialize_seq(TextVisitor)
}

fn extract(db: &Connection, source: &ArtifactId) -> anyhow::Result<String> {
    ensure!(
        db.is_autocommit(),
        "legacy visible text extraction cannot hold a transaction"
    );
    let length = artifact_length(db, source)?;
    let extra: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM chunks WHERE artifact=?1 AND (ordinal<0 OR ordinal>=?2))",
        params![source, length.div_ceil(CHUNK)],
        |r| r.get(0),
    )?;
    ensure!(!extra, "visible text source has unexpected chunks");
    let bytes = read_range(db, source, length, 0, length)?;
    let message: VisibleMessage =
        serde_json::from_slice(&bytes).context("corrupt assistant visible text source")?;
    ensure!(
        message.role == Role::Assistant,
        "visible text source is not an assistant message"
    );
    Ok(message.content)
}

struct CachedText {
    source: ArtifactId,
    text: String,
}

impl CachedText {
    fn bytes(&self) -> usize {
        self.source.0.capacity() + self.text.capacity()
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct CacheStats {
    pub retained_bytes: usize,
    // Conservative high-water reservation includes serde's transient allocations.
    pub high_water_bytes: usize,
    pub extractions: usize,
}

// Own on one bounded read worker for one database, never on the execution/UI thread.
pub(crate) struct Cache {
    entries: VecDeque<CachedText>,
    byte_limit: usize,
    entry_limit: usize,
    stats: CacheStats,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_limits(CACHE_BYTES, CACHE_ENTRIES)
    }
}

impl Cache {
    pub(crate) fn with_limits(byte_limit: usize, entry_limit: usize) -> Self {
        let byte_limit = byte_limit.min(CACHE_BYTES);
        let entry_limit = entry_limit
            .min(CACHE_ENTRIES)
            .min(byte_limit / std::mem::size_of::<CachedText>());
        let entries = VecDeque::with_capacity(entry_limit);
        let retained_bytes = entries.capacity() * std::mem::size_of::<CachedText>();
        Self {
            entries,
            byte_limit,
            entry_limit,
            stats: CacheStats {
                retained_bytes,
                high_water_bytes: retained_bytes,
                extractions: 0,
            },
        }
    }

    #[cfg(test)]
    fn stats(&self) -> CacheStats {
        self.stats
    }

    pub(crate) fn page(
        &mut self,
        db: &Connection,
        source: &ArtifactId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        if let Some(page) = page(db, source, offset, limit)? {
            return Ok(page);
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.source == source)
        {
            let entry = self.entries.remove(index).expect("located cache entry");
            let page = text_page(&entry.text, offset, limit);
            self.entries.push_back(entry);
            return page;
        }
        ensure!(
            self.entry_limit > 0 && self.byte_limit > 0,
            "legacy visible text cache is disabled"
        );
        ensure!(
            source.0.len() <= 256,
            "visible text source identity is oversized"
        );
        self.stats.high_water_bytes = self
            .stats
            .high_water_bytes
            .max(self.stats.retained_bytes + EXTRACTION_RESERVATION);
        let text = extract(db, source)?;
        self.stats.extractions += 1;
        let entry = CachedText {
            source: source.clone(),
            text,
        };
        let bytes = entry.bytes();
        let storage_bytes = self.entries.capacity() * std::mem::size_of::<CachedText>();
        ensure!(
            bytes <= self.byte_limit.saturating_sub(storage_bytes),
            "legacy visible text exceeds cache byte budget"
        );
        while self.entries.len() >= self.entry_limit
            || self.stats.retained_bytes + bytes > self.byte_limit
        {
            let evicted = self
                .entries
                .pop_front()
                .context("visible text cache accounting mismatch")?;
            self.stats.retained_bytes -= evicted.bytes();
        }
        let page = text_page(&entry.text, offset, limit);
        self.stats.retained_bytes += bytes;
        self.entries.push_back(entry);
        page
    }
}

#[cfg(test)]
#[path = "visible_text_tests.rs"]
mod tests;
