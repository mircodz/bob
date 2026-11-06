use crate::history::Source;
use crate::model::ThreadId;
use anyhow::{ensure, Context};
use bob_types::{Message, Role};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const PREVIEW_BYTES: usize = 8192;

pub(crate) struct Projection {
    pub(crate) source: Source,
    pub(crate) limitation: Option<&'static str>,
    pub(crate) submission: Option<String>,
}

pub(crate) fn visible(kind: &str) -> bool {
    matches!(kind, "assistant" | "submission" | "coordination")
}

fn fallback(text: String, code: &'static str) -> Projection {
    Projection {
        source: Source::text(text, "entries.text", true),
        limitation: Some(code),
        submission: None,
    }
}

pub(crate) fn project(
    db: &Connection,
    target: &ThreadId,
    sequence: i64,
    remaining: &mut usize,
) -> anyhow::Result<Option<Projection>> {
    if *remaining < PREVIEW_BYTES {
        return Ok(None);
    }
    let (kind, preview, model, operation, submission): (String, String, Option<String>, Option<String>, Option<String>) = db.query_row(
        "SELECT kind,substr(text,1,2048),CASE WHEN kind='assistant' THEN COALESCE(model,result) ELSE model END,operation,submission FROM entries WHERE thread=?1 AND seq=?2",
        params![target, sequence], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?.context("entry not found in target thread")?;
    ensure!(visible(&kind), "entry is not visible conversation text");
    *remaining -= preview.len();
    if let Some(operation) = &operation {
        let owned: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND thread=?2)",
            params![operation, target],
            |r| r.get(0),
        )?;
        ensure!(
            owned,
            "entry operation does not belong to the target thread"
        );
    }
    let linked: Option<String> = if let Some(id) = &submission {
        db.query_row(
            "SELECT s.id FROM submissions s JOIN threads recipient ON recipient.id=s.thread JOIN threads target ON target.id=?2 AND target.session=recipient.session WHERE s.id=?1 AND (s.thread=?2 OR (s.sender=?2 AND EXISTS(SELECT 1 FROM coordination c WHERE c.source=?2 AND c.destination=s.thread AND c.submission=s.id)))",
            params![id, target], |r| r.get(0),
        ).optional()?
    } else if kind == "coordination" {
        db.query_row(
            "SELECT s.id FROM coordination c JOIN submissions s ON s.id=c.submission AND s.thread=c.destination AND s.sender=c.source JOIN threads dest ON dest.id=c.destination JOIN threads src ON src.id=c.source AND src.session=dest.session JOIN operations o ON o.id=c.operation AND o.thread=c.source AND o.kind='tool' WHERE c.source=?1 AND c.operation=?2 ORDER BY c.rowid DESC LIMIT 1",
            params![target, operation], |r| r.get(0),
        ).optional()?
    } else {
        None
    };
    if let Some(id) = linked {
        let size: usize = db.query_row(
            "SELECT length(CAST(text AS BLOB)) FROM submissions WHERE id=?1",
            [&id],
            |r| r.get(0),
        )?;
        if size > MAX_SOURCE_BYTES {
            let mut projected = fallback(preview, "source_oversized");
            projected.submission = Some(id);
            return Ok(Some(projected));
        }
        if size > *remaining {
            return Ok(None);
        }
        let row: (Option<String>, Option<String>, String, String) = db.query_row(
            "SELECT CASE WHEN length(CAST(text AS BLOB))<=?2 THEN text END,sender,thread,source FROM submissions WHERE id=?1", params![id, size],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        )?;
        let Some(text) = row.0 else {
            let mut projected = fallback(preview, "source_oversized");
            projected.submission = Some(id);
            return Ok(Some(projected));
        };
        *remaining -= text.len();
        return Ok(Some(Projection {
            source: Source::submitted(
                (text, row.1, row.2, row.3),
                if submission.is_some() {
                    "submissions.text"
                } else {
                    "coordination.submission"
                },
            ),
            limitation: None,
            submission: Some(id),
        }));
    }
    if submission.is_some() {
        return Ok(Some(fallback(preview, "source_missing")));
    }
    if kind == "coordination" && operation.is_some() {
        let size: Option<usize> = db.query_row(
            "SELECT length(CAST(input AS BLOB)) FROM operations WHERE id=?1 AND thread=?2 AND kind='tool' AND name='send_message'",
            params![operation, target], |r| r.get(0),
        ).optional()?;
        if let Some(size) = size {
            if size > MAX_SOURCE_BYTES {
                return Ok(Some(fallback(preview, "source_oversized")));
            }
            if size > *remaining {
                return Ok(None);
            }
            let input: Option<String> = db.query_row("SELECT CASE WHEN length(CAST(input AS BLOB))<=?2 THEN input END FROM operations WHERE id=?1", params![operation, size], |r| r.get(0))?;
            let Some(input) = input else {
                return Ok(Some(fallback(preview, "source_oversized")));
            };
            *remaining -= input.len();
            if let Ok(input) = serde_json::from_str::<serde_json::Value>(&input) {
                if let Some(text) = input
                    .get("message")
                    .or_else(|| input.get("text"))
                    .and_then(|v| v.as_str())
                {
                    return Ok(Some(Projection {
                        source: Source::text(text.to_owned(), "operations.input.message", false),
                        limitation: None,
                        submission: None,
                    }));
                }
            }
            return Ok(Some(fallback(preview, "source_corrupt")));
        }
    }
    let Some(id) = model else {
        return Ok(Some(fallback(preview, "preview_only")));
    };
    let size: Option<i64> = db
        .query_row("SELECT length FROM artifacts WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(size) = size else {
        return Ok(Some(fallback(preview, "source_missing")));
    };
    if size < 0 || size as u64 > MAX_SOURCE_BYTES as u64 {
        return Ok(Some(fallback(preview, "source_oversized")));
    }
    if size as usize > *remaining {
        return Ok(None);
    }
    let text = visible_message(
        db,
        &id,
        size as usize,
        if kind == "assistant" {
            Role::Assistant
        } else {
            Role::User
        },
        remaining,
    );
    Ok(Some(match text {
        Ok(text) => Projection {
            source: Source::text(text, "message.text", false),
            limitation: None,
            submission: None,
        },
        Err(_) => fallback(preview, "source_corrupt"),
    }))
}

fn visible_message(
    db: &Connection,
    id: &str,
    size: usize,
    role: Role,
    remaining: &mut usize,
) -> anyhow::Result<String> {
    let mut stmt = db.prepare(
        "SELECT ordinal,length(data) FROM chunks WHERE artifact=?1 ORDER BY ordinal LIMIT 129",
    )?;
    let mut rows = stmt.query([id])?;
    let mut bytes = Vec::with_capacity(size);
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let ordinal: usize = row.get(0)?;
        let length: usize = row.get(1)?;
        ensure!(
            count < 128
                && ordinal == count
                && length <= 64 * 1024
                && length <= size.saturating_sub(bytes.len()),
            "invalid retained message chunks"
        );
        let chunk: Vec<u8> = db.query_row(
            "SELECT substr(data,1,?3) FROM chunks WHERE artifact=?1 AND ordinal=?2 AND length(data)=?3",
            params![id, ordinal, length], |r| r.get(0),
        )?;
        *remaining -= chunk.len();
        bytes.extend_from_slice(&chunk);
        count += 1;
    }
    ensure!(bytes.len() == size, "incomplete retained message");
    let message: Message = serde_json::from_slice(&bytes)?;
    ensure!(message.role == role, "entry is not a visible message");
    Ok(message.text())
}

pub(crate) fn canonical(
    db: &Connection,
    target: &ThreadId,
    submission: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(submission) = submission else {
        return Ok(true);
    };
    let recipient: String = db.query_row(
        "SELECT thread FROM submissions WHERE id=?1",
        [submission],
        |r| r.get(0),
    )?;
    if recipient == target.0 {
        return Ok(true);
    }
    let retained: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries WHERE thread=?1 AND submission=?2 AND kind IN ('submission','coordination'))",
        params![recipient, submission],
        |row| row.get(0),
    )?;
    Ok(!retained)
}
