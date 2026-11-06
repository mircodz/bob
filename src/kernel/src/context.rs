use crate::contracts::*;
use crate::model::*;
use crate::store::*;
use anyhow::ensure;
use bob_types::{ContentBlock, Message};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;

pub fn estimate_input_tokens(request: &RequestSpec) -> anyhow::Result<usize> {
    Ok(estimate_text(&request.system)
        + estimate_text(&encode(&request.tools)?)
        + request.messages.iter().map(estimate_message).sum::<usize>()
        + 16)
}
pub fn request_budget(model: &ModelInfo, capabilities: &ModelCapabilities) -> ContextBudget {
    let output = capabilities
        .preferred_output_tokens
        .min(capabilities.max_output_tokens)
        .min((model.context_window / 4).max(1).min(u32::MAX as usize) as u32)
        .max(1);
    let input = input_limit(model, capabilities, output);
    ContextBudget {
        context_window: model.context_window,
        input_limit: input,
        output_reserve: output,
        ..Default::default()
    }
}
pub(crate) fn input_limit(model: &ModelInfo, caps: &ModelCapabilities, output: u32) -> usize {
    model
        .context_window
        .saturating_sub(output as usize + (model.context_window / 20).max(32))
        .min(caps.max_input_tokens.unwrap_or(usize::MAX))
}

fn last_measurement(db: &Connection, thread: &ThreadId) -> anyhow::Result<Option<(u64, usize)>> {
    Ok(db.query_row(
        "SELECT json_extract(o.input,'$.measured_input_tokens'),json_extract(o.input,'$.estimated_input_tokens') FROM operations o JOIN contexts c ON c.thread=o.thread JOIN threads t ON t.id=o.thread WHERE o.thread=?1 AND o.name='model' AND o.state='done' AND o.epoch=c.epoch AND o.profile_revision=t.profile_revision AND json_extract(o.input,'$.checkpoint') IS c.checkpoint AND json_extract(o.input,'$.measured_input_tokens') IS NOT NULL AND json_extract(o.input,'$.estimated_input_tokens') IS NOT NULL ORDER BY o.rowid DESC LIMIT 1",
        [thread],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
}

pub(crate) fn measured_input(
    db: &Connection,
    thread: &ThreadId,
    estimated: usize,
) -> anyhow::Result<Option<u64>> {
    Ok(last_measurement(db, thread)?.map(|(measured, previous)| {
        measured.saturating_add(estimated.saturating_sub(previous) as u64)
    }))
}

pub(crate) fn estimate_text(text: &str) -> usize {
    text.len().div_ceil(4)
}
pub(crate) fn estimate_message(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => estimate_text(text),
            ContentBlock::ToolUse { id, name, input } => {
                estimate_text(id) + estimate_text(name) + estimate_text(&input.to_string())
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => estimate_text(tool_use_id) + estimate_text(&content.model_text()),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => estimate_text(thinking) + estimate_text(signature),
            ContentBlock::RedactedThinking { data } => estimate_text(data),
            ContentBlock::ReasoningItem { item } => estimate_text(&item.to_string()),
        })
        .sum::<usize>()
        + 4
}
#[cfg(test)]
pub(crate) fn working_ids(db: &Connection, thread: &ThreadId) -> anyhow::Result<Vec<ArtifactId>> {
    let mut stmt = db.prepare("SELECT model FROM working WHERE thread=?1 ORDER BY ordinal")?;
    let ids = stmt
        .query_map([thread], |r| Ok(ArtifactId(r.get(0)?)))?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}
pub(crate) fn snapshot(
    db: &Connection,
    thread: &ThreadId,
    model: &ModelInfo,
    caps: &ModelCapabilities,
) -> anyhow::Result<ContextSnapshot> {
    let (epoch,revision,profile_revision,checkpoint,pending):(i64,i64,i64,Option<String>,bool)=db.query_row("SELECT c.epoch,c.revision,t.profile_revision,c.checkpoint,c.compact_pending FROM contexts c JOIN threads t ON t.id=c.thread WHERE c.thread=?1",[thread],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let (message_count, estimated): (usize, usize) = db.query_row(
        "SELECT COUNT(*),COALESCE(SUM(tokens),0) FROM working WHERE thread=?1",
        [thread],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut budget = request_budget(model, caps);
    budget.estimated_input_tokens = estimated;
    budget.measured_input_tokens = last_measurement(db, thread)?.map(|(measured, _)| measured);
    let profile: Profile = decode(db.query_row(
        "SELECT profile FROM threads WHERE id=?1",
        [thread],
        |r| r.get(0),
    )?)?;
    Ok(ContextSnapshot {
        profile,
        thread: thread.clone(),
        epoch,
        revision,
        profile_revision,
        message_count,
        checkpoint: checkpoint.map(OperationId),
        compact_pending: pending,
        budget,
    })
}
pub(crate) fn safe_boundaries(messages: &[Message]) -> anyhow::Result<Vec<bool>> {
    let mut open = HashSet::new();
    let mut boundaries = vec![true];
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    ensure!(
                        open.insert(id.clone()),
                        "duplicate tool call in working context"
                    );
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    ensure!(
                        open.remove(tool_use_id),
                        "orphan tool result in working context"
                    );
                }
                _ => {}
            }
        }
        boundaries.push(open.is_empty());
    }
    Ok(boundaries)
}
pub(crate) fn checkpoint(
    db: &Connection,
    thread: &ThreadId,
    operation: &OperationId,
    epoch: i64,
    revision: i64,
    messages: &[Message],
) -> anyhow::Result<()> {
    ensure!(db.execute("UPDATE contexts SET revision=revision+1,checkpoint=?1,compact_pending=0 WHERE thread=?2 AND epoch=?3 AND revision=?4",params![operation,thread,epoch,revision])?==1,"compaction snapshot is stale");
    let mut ids = Vec::new();
    for message in messages {
        ids.push(artifact(
            db,
            &encode(&ModelRecord::Message(message.clone()))?,
        )?);
    }
    let manifest = artifact(db, &encode(&ids)?)?;
    db.execute(
        "INSERT INTO checkpoints(operation,thread,epoch,revision,manifest) VALUES(?1,?2,?3,?4,?5)",
        params![operation, thread, epoch, revision + 1, manifest],
    )?;
    db.execute("DELETE FROM working WHERE thread=?1", [thread])?;
    for (id, message) in ids.into_iter().zip(messages) {
        db.execute(
            "INSERT INTO working(thread,model,tokens) VALUES(?1,?2,?3)",
            params![thread, id, estimate_message(message)],
        )?;
    }
    Ok(())
}
