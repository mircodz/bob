use crate::model::{OperationId, PreparedAuthorizationRequirement, PreparedTool, SessionId};
use anyhow::{ensure, Context};
use rusqlite::{params, Transaction};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS effect_claims(operation TEXT NOT NULL REFERENCES operations(id), resource TEXT NOT NULL, session TEXT NOT NULL REFERENCES sessions(id), generation TEXT NOT NULL, writer INTEGER NOT NULL, PRIMARY KEY(operation,resource));
CREATE INDEX IF NOT EXISTS effect_claim_session ON effect_claims(session);
INSERT OR IGNORE INTO effect_claims(operation,resource,session,generation,writer)
SELECT o.id,s.workspace,s.id,'legacy',o.effect_write FROM operations o JOIN threads t ON t.id=o.thread JOIN sessions s ON s.id=t.session
WHERE o.kind='tool' AND (o.state IN ('started','unknown') OR EXISTS(SELECT 1 FROM jobs j WHERE j.operation=o.id AND j.kind='shell' AND j.state IN ('queued','running','stopping','unknown')));
INSERT OR IGNORE INTO effect_claims(operation,resource,session,generation,writer)
SELECT o.id,json_extract(a.value,'$.path'),t.session,'legacy',o.effect_write FROM operations o JOIN threads t ON t.id=o.thread,json_each(o.authorizations) a
WHERE json_type(a.value,'$.path')='text' AND o.kind='tool' AND (o.state IN ('started','unknown') OR EXISTS(SELECT 1 FROM jobs j WHERE j.operation=o.id AND j.kind='shell' AND j.state IN ('queued','running','stopping','unknown')));
";

pub(crate) fn canonical_resource(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(path.is_absolute(), "effect resource must be absolute");
    let mut existing = path.to_owned();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .context("resource has no existing ancestor")?
                .to_os_string(),
        );
        ensure!(existing.pop(), "resource has no existing ancestor");
    }
    let mut canonical = std::fs::canonicalize(existing)?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

pub(crate) fn resources(
    tool: &PreparedTool,
    requirements: &[PreparedAuthorizationRequirement],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut resources = vec![canonical_resource(Path::new(&tool.invocation.workspace))?];
    for requirement in requirements {
        let path = match requirement {
            PreparedAuthorizationRequirement::EditFile { path }
            | PreparedAuthorizationRequirement::ToolPath { path } => path,
        };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            Path::new(&tool.invocation.workspace).join(path)
        };
        resources.push(canonical_resource(&path)?);
    }
    resources.sort();
    resources.dedup();
    Ok(resources)
}

pub(crate) fn release_settled(tx: &Transaction<'_>, operation: &OperationId) -> anyhow::Result<()> {
    tx.execute("DELETE FROM effect_claims WHERE operation=?1 AND EXISTS(SELECT 1 FROM operations o WHERE o.id=?1 AND o.state IN ('done','failed','cancelled','resolved')) AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.operation=?1 AND j.kind='shell' AND j.state IN ('queued','running','stopping','unknown'))", [operation])?;
    Ok(())
}

pub(crate) fn claim(
    tx: &Transaction<'_>,
    operation: &OperationId,
    session: &SessionId,
    generation: &str,
    resources: &[PathBuf],
    writer: bool,
) -> anyhow::Result<bool> {
    // Only in-flight effects conflict; `unknown` outcomes and background jobs never block peers.
    let mut stmt = tx.prepare("SELECT c.resource,c.writer,c.session FROM effect_claims c JOIN operations o ON o.id=c.operation WHERE c.operation!=?1 AND o.state='started'")?;
    let claims = stmt.query_map(params![operation], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for claim in claims {
        let (resource, other_writer, owner) = claim?;
        if (writer || other_writer)
            && resources
                .iter()
                .any(|path| path.starts_with(&resource) || Path::new(&resource).starts_with(path))
            && (owner == session.0 || owner_alive(tx, owner))
        {
            return Ok(false);
        }
    }
    drop(stmt);
    for resource in resources {
        tx.execute("INSERT INTO effect_claims(operation,resource,session,generation,writer) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(operation,resource) DO UPDATE SET generation=excluded.generation,writer=excluded.writer", params![operation, resource.to_string_lossy(), session, generation, writer])?;
    }
    Ok(true)
}

/// A `started` claim from a session whose process died is not in flight: its worker was torn down.
fn owner_alive(tx: &Transaction<'_>, session: String) -> bool {
    tx.path()
        .filter(|path| !path.is_empty())
        .and_then(|path| crate::session_in_use(Path::new(path), &SessionId(session)))
        .unwrap_or(true)
}
