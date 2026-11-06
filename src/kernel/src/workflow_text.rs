use crate::workflow_contracts::*;
use crate::{ArtifactId, OperationId, SessionId};
use anyhow::{ensure, Context};
use rusqlite::{params, Connection};
use serde::Deserialize;

pub(super) fn definition(
    db: &Connection,
    operation: &OperationId,
) -> anyhow::Result<serde_json::Value> {
    let (length, text): (usize, Option<String>) = db.query_row(
        "SELECT length(CAST(input AS BLOB)), CASE WHEN length(CAST(input AS BLOB))<=?2 THEN input END FROM operations WHERE id=?1",
        params![operation, WORKFLOW_MAX_DEFINITION_BYTES], |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        length <= WORKFLOW_MAX_DEFINITION_BYTES,
        "definition exceeds the 4 MiB inspection bound; use definition text pages"
    );
    serde_json::from_str(&text.context("definition unavailable")?)
        .context("invalid workflow definition")
}

pub(super) fn linked(
    db: &Connection,
    operation: &OperationId,
    artifact: &ArtifactId,
) -> anyhow::Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflows WHERE operation=?1 AND result=?2
         UNION ALL SELECT 1 FROM operations WHERE id=?1 AND name='workflow' AND result=?2
         UNION ALL SELECT 1 FROM workflow_nodes WHERE operation=?1 AND result=?2
         UNION ALL SELECT 1 FROM workflow_iterations WHERE operation=?1 AND result=?2
         UNION ALL SELECT 1 FROM agents a JOIN threads t ON t.id=a.thread
             JOIN operations o ON o.id=?1 JOIN threads owner ON owner.id=o.thread
             WHERE t.session=owner.session AND a.outcome=?2 AND (a.operation=?1 OR EXISTS(
                 SELECT 1 FROM workflow_nodes n WHERE n.operation=?1 AND n.child=a.thread)))",
        params![operation, artifact],
        |r| r.get(0),
    )?)
}

pub(super) fn page(
    db: &Connection,
    session: &SessionId,
    operation: &OperationId,
    source: &WorkflowTextSource,
    offset: usize,
    max_bytes: usize,
) -> anyhow::Result<WorkflowTextPage> {
    ensure!(
        (1024..=WORKFLOW_MAX_PAGE_BYTES).contains(&max_bytes),
        "text max_bytes must be 1024..65536"
    );
    let budget = (max_bytes - 768) / 6;
    let (total_bytes, text) = match source {
        WorkflowTextSource::Artifact { artifact } => {
            ensure!(
                linked(db, operation, artifact)?,
                "artifact is not an allowed workflow result"
            );
            let length = db.query_row(
                "SELECT length FROM artifacts WHERE id=?1",
                [artifact],
                |r| r.get(0),
            )?;
            (
                length,
                crate::store::artifact_page(db, artifact, offset, budget)?,
            )
        }
        WorkflowTextSource::Definition => column_page(db, operation, None, offset, budget)?,
        WorkflowTextSource::NodeSpecification { path } => {
            super::workflow_view::valid_path(path)?;
            let declared = definition(db, operation)
                .ok()
                .and_then(|value| super::workflow_definition::find(&value, path));
            let stored: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_nodes WHERE operation=?1 AND path=?2)",
                params![operation, path],
                |r| r.get(0),
            )?;
            if stored && declared.is_none() {
                column_page(db, operation, Some(path), offset, budget)?
            } else {
                let specification = match declared {
                    Some(declared) => declared.specification,
                    None => super::workflow_view::synthetic_specification(db, operation, path)?,
                };
                let text = serde_json::to_string(&specification)?;
                ensure!(
                    offset <= text.len() && text.is_char_boundary(offset),
                    "invalid UTF-8 text offset"
                );
                let mut end = offset.saturating_add(budget).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                (text.len(), text[offset..end].to_owned())
            }
        }
    };
    let next = offset + text.len();
    let result = WorkflowTextPage {
        session: session.clone(),
        operation: operation.clone(),
        source: source.clone(),
        offset,
        text,
        next_offset: (next < total_bytes).then_some(next),
        total_bytes,
    };
    ensure!(
        serde_json::to_vec(&result)?.len() <= max_bytes,
        "text response exceeds byte budget"
    );
    Ok(result)
}

fn column_page(
    db: &Connection,
    operation: &OperationId,
    path: Option<&str>,
    offset: usize,
    budget: usize,
) -> anyhow::Result<(usize, String)> {
    let sql = if path.is_some() {
        "SELECT length(CAST(specification AS BLOB)),substr(CAST(specification AS BLOB),?3+1,?4) FROM workflow_nodes WHERE operation=?1 AND path=?2"
    } else {
        "SELECT length(CAST(input AS BLOB)),substr(CAST(input AS BLOB),?3+1,?4) FROM operations WHERE id=?1 AND ?2 IS NULL"
    };
    let (length, mut bytes): (usize, Vec<u8>) =
        db.query_row(sql, params![operation, path, offset, budget], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    ensure!(offset <= length, "text offset exceeds retained length");
    if let Err(error) = std::str::from_utf8(&bytes) {
        ensure!(
            error.error_len().is_none(),
            "text offset is not a UTF-8 boundary"
        );
        bytes.truncate(error.valid_up_to());
    }
    Ok((length, String::from_utf8(bytes)?))
}

#[derive(Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    output: OutputMetadata,
}
#[derive(Default, Deserialize)]
struct OutputMetadata {
    #[serde(default, rename = "_stop_reason")]
    stop_reason: Option<String>,
    #[serde(default)]
    termination_reason: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

struct ArtifactReader<'a> {
    db: &'a Connection,
    artifact: &'a ArtifactId,
    offset: usize,
    length: usize,
}
impl std::io::Read for ArtifactReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.length || out.is_empty() {
            return Ok(0);
        }
        let count = out
            .len()
            .min(8192)
            .min(self.length - self.offset)
            .min(65536 - self.offset % 65536);
        let bytes: Vec<u8> = self
            .db
            .query_row(
                "SELECT substr(data,?3+1,?4) FROM chunks WHERE artifact=?1 AND ordinal=?2",
                params![
                    self.artifact,
                    self.offset / 65536,
                    self.offset % 65536,
                    count
                ],
                |r| r.get(0),
            )
            .map_err(std::io::Error::other)?;
        if bytes.is_empty() {
            return Err(std::io::Error::other("missing artifact bytes"));
        }
        out[..bytes.len()].copy_from_slice(&bytes);
        self.offset += bytes.len();
        Ok(bytes.len())
    }
}

pub(super) fn metadata(
    db: &Connection,
    artifact: &ArtifactId,
) -> anyhow::Result<(Option<WorkflowTermination>, Option<String>)> {
    let length: usize = db.query_row(
        "SELECT length FROM artifacts WHERE id=?1",
        [artifact],
        |r| r.get(0),
    )?;
    ensure!(
        length <= WORKFLOW_MAX_DEFINITION_BYTES,
        "result exceeds retained artifact bound"
    );
    // Ignored output fields stream through a fixed-size buffer, never a full result allocation.
    let reader = std::io::BufReader::new(ArtifactReader {
        db,
        artifact,
        offset: 0,
        length,
    });
    let Ok(value) = serde_json::from_reader::<_, Metadata>(reader) else {
        return Ok((None, None));
    };
    let termination = value
        .output
        .stop_reason
        .or(value.output.termination_reason)
        .map(|reason| match reason.as_str() {
            "condition_met" => WorkflowTermination::ConditionMet,
            "max_rounds" => WorkflowTermination::MaxRounds,
            "dry" => WorkflowTermination::Dry,
            _ => WorkflowTermination::Unknown,
        });
    Ok((
        termination,
        value
            .output
            .branch
            .filter(|branch| matches!(branch.as_str(), "then" | "else")),
    ))
}
