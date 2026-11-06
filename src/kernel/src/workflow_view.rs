use crate::workflow_contracts::*;
use crate::workflow_definition::{self, Declared};
use crate::workflow_text;
use crate::{ArtifactId, JobId, OperationId, SessionId, ThreadId};
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

type SummaryRow = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    usize,
    usize,
    Option<String>,
);
type NodeRow = (
    String,
    usize,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    scope: String,
    head: i64,
    position: i64,
    key: String,
}

pub(crate) fn query(
    db: &Connection,
    session: &SessionId,
    query: &WorkflowQuery,
) -> anyhow::Result<WorkflowQueryResult> {
    uuid::Uuid::parse_str(&session.0).context("invalid workflow session ID")?;
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [session],
            |r| r.get::<_, bool>(0)
        )?,
        "unknown workflow session"
    );
    if let WorkflowQuery::List { page } = query {
        let result = WorkflowQueryResult::List(list(db, session, page)?);
        ensure!(
            serde_json::to_vec(&result)?.len() <= page.max_bytes,
            "workflow response exceeds requested byte budget"
        );
        return Ok(result);
    }
    let operation = match query {
        WorkflowQuery::Overview { operation, .. }
        | WorkflowQuery::Node { operation, .. }
        | WorkflowQuery::Iterations { operation, .. }
        | WorkflowQuery::Agents { operation, .. }
        | WorkflowQuery::Text { operation, .. } => operation,
        WorkflowQuery::List { .. } => unreachable!(),
    };
    uuid::Uuid::parse_str(&operation.0).context("invalid workflow operation ID")?;
    ensure!(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations o JOIN threads t ON t.id=o.thread WHERE o.id=?1 AND o.name='workflow' AND o.kind='tool' AND t.session=?2)",
        params![operation, session], |r| r.get::<_, bool>(0),
    )?, "workflow is not in this session");
    let result = match query {
        WorkflowQuery::Overview {
            container, page, ..
        } => WorkflowQueryResult::Overview(overview(
            db,
            session,
            operation,
            container.as_deref(),
            page,
        )?),
        WorkflowQuery::Node { path, .. } => {
            WorkflowQueryResult::Node(details(db, operation, path)?)
        }
        WorkflowQuery::Iterations { path, page, .. } => {
            WorkflowQueryResult::Iterations(iterations(db, session, operation, path, page)?)
        }
        WorkflowQuery::Agents { page, .. } => {
            WorkflowQueryResult::Agents(agents(db, session, operation, page)?)
        }
        WorkflowQuery::Text {
            source,
            offset,
            max_bytes,
            ..
        } => WorkflowQueryResult::Text(workflow_text::page(
            db, session, operation, source, *offset, *max_bytes,
        )?),
        WorkflowQuery::List { .. } => unreachable!(),
    };
    let max_bytes = match query {
        WorkflowQuery::Overview { page, .. }
        | WorkflowQuery::Iterations { page, .. }
        | WorkflowQuery::Agents { page, .. } => page.max_bytes,
        WorkflowQuery::Text { max_bytes, .. } => *max_bytes,
        _ => WORKFLOW_MAX_PAGE_BYTES,
    };
    ensure!(
        serde_json::to_vec(&result)?.len() <= max_bytes,
        "workflow response exceeds requested byte budget"
    );
    Ok(result)
}

pub(super) fn valid_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 1024
            && !path
                .split('/')
                .any(|part| part.is_empty() || part.starts_with('!')),
        "invalid workflow node path"
    );
    Ok(())
}

fn cursor(
    db: &Connection,
    session: &SessionId,
    operation: Option<&OperationId>,
    view: &str,
    path: Option<&str>,
    page: &WorkflowPageRequest,
    table: &str,
) -> anyhow::Result<Cursor> {
    ensure!(
        (1..=WORKFLOW_MAX_ROWS).contains(&page.limit),
        "workflow limit must be 1..100"
    );
    ensure!(
        (4096..=WORKFLOW_MAX_PAGE_BYTES).contains(&page.max_bytes),
        "workflow max_bytes must be 4096..65536"
    );
    let scope = crate::store::digest(&(session, operation, view, path))?;
    if let Some(text) = &page.cursor {
        ensure!(text.len() <= 4096, "workflow cursor too large");
        let cursor: Cursor = serde_json::from_str(text).context("invalid workflow cursor")?;
        ensure!(
            cursor.version == 1
                && cursor.scope == scope
                && cursor.head >= 0
                && cursor.position >= 0
                && (view == "overview" || cursor.key.is_empty())
                && cursor.key.len() <= 1024,
            "workflow cursor does not match request scope"
        );
        return Ok(cursor);
    }
    let head: i64 = db.query_row(
        &format!("SELECT COALESCE(MAX(rowid),0) FROM {table}"),
        [],
        |r| r.get(0),
    )?;
    Ok(Cursor {
        version: 1,
        scope,
        head,
        position: 0,
        key: String::new(),
    })
}

fn push<T: Serialize>(
    rows: &mut Vec<T>,
    value: T,
    used: &mut usize,
    page: &WorkflowPageRequest,
) -> anyhow::Result<bool> {
    let size = serde_json::to_vec(&value)?.len() + 1;
    if *used + size > page.max_bytes {
        ensure!(
            !rows.is_empty(),
            "workflow row cannot fit requested byte budget"
        );
        return Ok(false);
    }
    *used += size;
    rows.push(value);
    Ok(true)
}
fn finish<T>(rows: Vec<T>, cursor: &Cursor, more: bool) -> anyhow::Result<WorkflowPage<T>> {
    Ok(WorkflowPage {
        rows,
        next_cursor: more.then(|| serde_json::to_string(cursor)).transpose()?,
    })
}

fn raw_state(raw: &str) -> WorkflowState {
    match raw {
        "completed" | "done" | "resolved" => WorkflowState::Completed,
        "failed" | "failing" => WorkflowState::Failed,
        "cancelled" => WorkflowState::Cancelled,
        "stopping" => WorkflowState::Stopping,
        "unknown" | "interrupted" => WorkflowState::Unknown,
        "approval" => WorkflowState::WaitingPermission,
        "waiting_capacity" => WorkflowState::WaitingCapacity,
        "queued" | "pending" | "ready" | "preparing" => WorkflowState::Pending,
        "running" | "active" | "started" | "waiting" => WorkflowState::Running,
        _ => WorkflowState::Unknown,
    }
}
fn terminal(state: WorkflowState) -> bool {
    matches!(
        state,
        WorkflowState::Completed
            | WorkflowState::Failed
            | WorkflowState::Cancelled
            | WorkflowState::Denied
            | WorkflowState::Skipped
            | WorkflowState::NotSelected
            | WorkflowState::Unknown
    )
}

fn summary(db: &Connection, operation: &OperationId) -> anyhow::Result<WorkflowSummary> {
    let (owner, job, title, op_state, workflow_state, job_state, run_state, decision, budget, executions, result):
        SummaryRow = db.query_row(
        "SELECT o.thread,w.job,COALESCE(substr(j.title,1,128),CASE WHEN json_valid(o.input) THEN substr(json_extract(o.input,'$.title'),1,128) END,'Workflow'),o.state,w.state,j.state,r.state,d.state,
        CASE WHEN json_valid(o.input) AND json_type(o.input,'$.max_concurrency')='integer' AND json_extract(o.input,'$.max_concurrency') BETWEEN 1 AND 64 THEN json_extract(o.input,'$.max_concurrency') ELSE 64 END,COALESCE(w.executions,0),COALESCE(w.result,o.result)
        FROM operations o JOIN runs r ON r.id=o.run LEFT JOIN workflows w ON w.operation=o.id LEFT JOIN jobs j ON j.id=w.job LEFT JOIN decisions d ON d.operation=o.id WHERE o.id=?1",
        [operation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?)),
    )?;
    let settled = matches!(
        workflow_state.as_deref(),
        Some("completed" | "failed" | "cancelled")
    );
    let (active_agents, unknown_children): (usize, bool) = if settled {
        (0, false)
    } else {
        db.query_row(
        "SELECT COALESCE(SUM(a.state IN ('queued','running','stopping') OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state='stopping')),0),COALESCE(MAX(a.state='unknown' OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state='interrupted')),0)
         FROM agents a JOIN threads t ON t.id=a.thread JOIN threads owner ON owner.id=?2 WHERE t.session=owner.session
         AND (a.operation=?1 OR EXISTS(SELECT 1 FROM workflow_nodes n WHERE n.operation=?1 AND n.child=a.thread))",
        params![operation, owner], |r| Ok((r.get(0)?,r.get(1)?)),
    )?
    };
    let admitted = workflow_state.is_some();
    let state = if let Some(workflow_state) = workflow_state {
        if settled {
            raw_state(&workflow_state)
        } else if job_state.as_deref() == Some("unknown")
            || workflow_state == "unknown"
            || unknown_children
        {
            WorkflowState::Unknown
        } else if job_state.as_deref() == Some("stopping") {
            WorkflowState::Stopping
        } else if job_state.as_deref() == Some("cancelled") || workflow_state == "cancelled" {
            WorkflowState::Cancelled
        } else if job_state.as_deref() == Some("failed") {
            WorkflowState::Failed
        } else if run_state == "stopping" {
            WorkflowState::Stopping
        } else if matches!(run_state.as_str(), "interrupted" | "unknown") {
            WorkflowState::Unknown
        } else if workflow_state == "failing" {
            WorkflowState::Failed
        } else {
            WorkflowState::Running
        }
    } else if decision.as_deref() == Some("denied") {
        WorkflowState::Denied
    } else if op_state == "approval" {
        WorkflowState::Approval
    } else if matches!(
        op_state.as_str(),
        "failed" | "cancelled" | "unknown" | "interrupted"
    ) {
        raw_state(&op_state)
    } else {
        WorkflowState::Unadmitted
    };
    Ok(WorkflowSummary {
        operation: operation.clone(),
        owner: ThreadId(owner),
        job: job.map(JobId),
        title,
        state,
        admitted,
        max_concurrency: budget,
        executions,
        active_agents,
        result: result.map(ArtifactId),
    })
}

fn list(
    db: &Connection,
    session: &SessionId,
    page: &WorkflowPageRequest,
) -> anyhow::Result<WorkflowPage<WorkflowSummary>> {
    let mut cursor = cursor(db, session, None, "list", None, page, "operations")?;
    let mut stmt = db.prepare("SELECT o.rowid,o.id FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND o.name='workflow' AND o.kind='tool' AND (?2=0 OR o.rowid<?2) AND o.rowid<=?3 ORDER BY o.rowid DESC LIMIT ?4")?;
    let candidates = stmt
        .query_map(
            params![session, cursor.position, cursor.head, page.limit + 1],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    let mut used = 2048;
    let mut more = false;
    for (ordinal, id) in candidates {
        if rows.len() == page.limit
            || !push(&mut rows, summary(db, &OperationId(id))?, &mut used, page)?
        {
            more = true;
            break;
        }
        cursor.position = ordinal;
    }
    finish(rows, &cursor, more)
}

fn child_state(db: &Connection, child: &ThreadId) -> anyhow::Result<WorkflowState> {
    let raw: Option<String> = db
        .query_row("SELECT state FROM agents WHERE thread=?1", [child], |r| {
            r.get(0)
        })
        .optional()?;
    let stopping: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state='stopping')",
        [child],
        |r| r.get(0),
    )?;
    if stopping {
        return Ok(WorkflowState::Stopping);
    }
    let state = raw
        .as_deref()
        .map(raw_state)
        .unwrap_or(WorkflowState::Unknown);
    if terminal(state) {
        return Ok(state);
    }
    let (approval, human, unknown): (bool,bool,bool) = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND state='approval'),EXISTS(SELECT 1 FROM human_controls h JOIN operations o ON o.id=h.operation WHERE o.thread=?1 AND h.state='pending'),EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state='interrupted')",
        [child], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    Ok(if unknown {
        WorkflowState::Unknown
    } else if approval {
        WorkflowState::WaitingPermission
    } else if human {
        WorkflowState::WaitingHuman
    } else {
        state
    })
}

fn choice(db: &Connection, operation: &OperationId, path: &str) -> anyhow::Result<Option<String>> {
    let artifact: Option<String> = db
        .query_row(
            "SELECT result FROM workflow_nodes WHERE operation=?1 AND path=?2",
            params![operation, format!("{path}/!choice")],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    artifact
        .map(|id| workflow_text::metadata(db, &ArtifactId(id)).map(|(_, branch)| branch))
        .transpose()
        .map(Option::flatten)
}
fn not_selected(db: &Connection, operation: &OperationId, path: &str) -> anyhow::Result<bool> {
    let parts: Vec<_> = path.split('/').collect();
    for index in 1..parts.len() {
        if matches!(parts[index], "then" | "else") {
            if let Some(branch) = choice(db, operation, &parts[..index].join("/"))? {
                if branch != parts[index] {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn descendant_state(
    db: &Connection,
    operation: &OperationId,
    path: &str,
) -> anyhow::Result<Option<WorkflowState>> {
    let start = format!("{path}/");
    let end = format!("{path}0");
    let (unknown, stopping, active, capacity, failed, cancelled, pending, total): (bool,bool,bool,bool,bool,bool,bool,usize) = db.query_row(
        "SELECT COALESCE(MAX(n.state='unknown' OR (n.state NOT IN ('completed','failed','cancelled') AND a.state='unknown')),0),COALESCE(MAX(n.state NOT IN ('completed','failed','cancelled','unknown') AND (a.state='stopping' OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=n.child AND r.state='stopping'))),0),COALESCE(MAX(n.state='running' OR (n.state NOT IN ('completed','failed','cancelled','unknown') AND a.state IN ('queued','running'))),0),COALESCE(MAX(n.state='waiting_capacity'),0),COALESCE(MAX(n.state='failed'),0),COALESCE(MAX(n.state='cancelled' OR (n.state NOT IN ('completed','failed','unknown') AND a.state='cancelled')),0),COALESCE(MAX(n.state NOT IN ('completed','failed','cancelled','unknown')),0),COUNT(*) FROM workflow_nodes n LEFT JOIN agents a ON a.thread=n.child WHERE n.operation=?1 AND n.path>=?2 AND n.path<?3 AND n.path NOT LIKE '%/!%'",
        params![operation,start,end], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)),
    )?;
    Ok(if unknown {
        Some(WorkflowState::Unknown)
    } else if stopping {
        Some(WorkflowState::Stopping)
    } else if active {
        Some(WorkflowState::Running)
    } else if capacity {
        Some(WorkflowState::WaitingCapacity)
    } else if failed {
        Some(WorkflowState::Failed)
    } else if cancelled {
        Some(WorkflowState::Cancelled)
    } else if pending {
        Some(WorkflowState::Pending)
    } else if total > 0 {
        Some(WorkflowState::Completed)
    } else {
        None
    })
}

fn project(
    db: &Connection,
    workflow: &WorkflowSummary,
    path: &str,
    declared: Option<&Declared>,
) -> anyhow::Result<WorkflowNode> {
    valid_path(path)?;
    let stored: Option<NodeRow> = db.query_row(
        "SELECT state,iteration,child,result,substr(error,1,256),CASE WHEN length(CAST(specification AS BLOB))<=4096 THEN specification WHEN json_valid(specification) AND json_type(specification,'$.prompt')='text' THEN '{\"prompt\":\"\"}' END FROM workflow_nodes WHERE operation=?1 AND path=?2",
        params![workflow.operation,path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
    ).optional()?;
    let specification = stored
        .as_ref()
        .and_then(|row| row.5.as_ref())
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let inferred = specification.map(|spec| workflow_definition::from_spec(path.into(), spec));
    let declared = declared.or(inferred.as_ref());
    let kind = declared
        .map(|node| node.kind)
        .unwrap_or(WorkflowNodeKind::Unknown);
    let descendant = descendant_state(db, &workflow.operation, path)?;
    ensure!(
        stored.is_some() || declared.is_some() || descendant.is_some(),
        "unknown workflow node"
    );
    let child = stored.as_ref().and_then(|row| row.2.clone()).map(ThreadId);
    if let Some(child) = &child {
        ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM threads child JOIN threads owner ON child.session=owner.session WHERE child.id=?1 AND owner.id=?2)",params![child,workflow.owner],|r|r.get::<_,bool>(0))?, "workflow child is outside the session");
    }
    let recorded = stored.as_ref().map(|row| raw_state(&row.0));
    let mut state = if let Some(recorded) = recorded.filter(|state| terminal(*state)) {
        recorded
    } else if let Some(child) = &child {
        child_state(db, child)?
    } else {
        recorded.unwrap_or(WorkflowState::Future)
    };
    if !terminal(state) {
        if let Some(descendant) = descendant {
            state = if descendant == WorkflowState::Completed && stored.is_some() {
                WorkflowState::Running
            } else {
                descendant
            };
        }
        if path == "find"
            && workflow.state == WorkflowState::Running
            && state == WorkflowState::Completed
        {
            state = WorkflowState::Running;
        }
    }
    if kind == WorkflowNodeKind::Branch
        && declared.is_some_and(|node| {
            node.specification["steps"]
                .as_array()
                .is_some_and(Vec::is_empty)
        })
    {
        if let Some((parent, branch)) = path.rsplit_once('/') {
            if choice(db, &workflow.operation, parent)?.as_deref() == Some(branch) {
                let complete: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM workflow_nodes WHERE operation=?1 AND path=?2 AND state='completed')",params![workflow.operation,parent],|r|r.get(0))?;
                if complete {
                    state = WorkflowState::Completed;
                }
            }
        }
    }
    if not_selected(db, &workflow.operation, path)? {
        state = WorkflowState::NotSelected;
    } else if !terminal(state) {
        state = match workflow.state {
            WorkflowState::Unknown => WorkflowState::Unknown,
            WorkflowState::Cancelled | WorkflowState::Stopping => workflow.state,
            WorkflowState::Failed | WorkflowState::Denied => {
                if stored.is_some() {
                    WorkflowState::Blocked
                } else {
                    WorkflowState::Skipped
                }
            }
            WorkflowState::Approval => WorkflowState::WaitingPermission,
            _ => state,
        };
        if let Some(dependency) =
            declared
                .and_then(|node| node.dependency.as_deref())
                .filter(|_| {
                    !matches!(
                        state,
                        WorkflowState::Cancelled | WorkflowState::Stopping | WorkflowState::Unknown
                    )
                })
        {
            let raw: Option<String> = db
                .query_row(
                    "SELECT state FROM workflow_nodes WHERE operation=?1 AND path=?2",
                    params![workflow.operation, dependency],
                    |r| r.get(0),
                )
                .optional()?;
            if raw
                .as_deref()
                .is_some_and(|raw| matches!(raw, "failed" | "cancelled" | "unknown"))
                || descendant_state(db, &workflow.operation, dependency)?
                    == Some(WorkflowState::Failed)
            {
                state = WorkflowState::Blocked;
            }
        }
    }
    let result = stored
        .as_ref()
        .and_then(|row| row.3.clone())
        .map(ArtifactId);
    let selected_branch = if kind == WorkflowNodeKind::Conditional {
        choice(db, &workflow.operation, path)?
    } else {
        None
    };
    let termination = if kind == WorkflowNodeKind::Loop {
        let source = result.as_ref().or_else(|| {
            (path == "find")
                .then_some(workflow.result.as_ref())
                .flatten()
        });
        source
            .map(|artifact| workflow_text::metadata(db, artifact).map(|meta| meta.0))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let iteration = if path == "find" {
        db.query_row("SELECT COALESCE(MAX(CAST(substr(path,10) AS INTEGER)),0) FROM workflow_nodes WHERE operation=?1 AND path GLOB 'find/pass[0-9]*'",[&workflow.operation],|r|r.get(0))?
    } else if kind == WorkflowNodeKind::Iteration {
        path.rsplit('/')
            .next()
            .and_then(|part| part.strip_prefix("pass"))
            .context("invalid iteration path")?
            .parse()?
    } else {
        stored.as_ref().map_or(0, |row| row.1)
    };
    Ok(WorkflowNode {
        path: path.into(),
        label: path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .chars()
            .take(128)
            .collect(),
        kind,
        state,
        instantiated: stored.is_some() || descendant.is_some(),
        expandable: matches!(
            kind,
            WorkflowNodeKind::FanOut
                | WorkflowNodeKind::Parallel
                | WorkflowNodeKind::Loop
                | WorkflowNodeKind::Conditional
                | WorkflowNodeKind::Branch
                | WorkflowNodeKind::Iteration
        ) || descendant.is_some(),
        iteration,
        child,
        result,
        error_preview: stored.and_then(|row| row.4),
        termination,
        selected_branch,
    })
}

fn overview(
    db: &Connection,
    session: &SessionId,
    operation: &OperationId,
    container: Option<&str>,
    page: &WorkflowPageRequest,
) -> anyhow::Result<WorkflowOverview> {
    if let Some(path) = container {
        valid_path(path)?;
    }
    let mut cursor = cursor(
        db,
        session,
        Some(operation),
        "overview",
        container,
        page,
        "workflow_nodes",
    )?;
    let workflow = summary(db, operation)?;
    let (definition, definition_error) = match workflow_text::definition(db, operation) {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string().chars().take(256).collect())),
    };
    if let Some(path) = container {
        let declared = definition
            .as_ref()
            .and_then(|value| workflow_definition::find(value, path));
        project(db, &workflow, path, declared.as_ref())?;
    }
    let declarations = definition
        .as_ref()
        .map(|value| workflow_definition::children(value, container))
        .unwrap_or_default();
    ensure!(
        cursor.position as usize <= declarations.len(),
        "workflow definition changed; restart pagination"
    );
    let mut rows = Vec::new();
    let mut used = 3072;
    let mut more = false;
    for (index, declaration) in declarations
        .iter()
        .enumerate()
        .skip(cursor.position as usize)
    {
        if rows.len() == page.limit
            || !push(
                &mut rows,
                project(db, &workflow, &declaration.path, Some(declaration))?,
                &mut used,
                page,
            )?
        {
            more = true;
            break;
        }
        cursor.position = (index + 1) as i64;
    }
    if !more {
        let prefix = container.map(|path| format!("{path}/")).unwrap_or_else(|| {
            if definition
                .as_ref()
                .is_some_and(|value| value.get("steps").is_some())
            {
                "root/".into()
            } else {
                String::new()
            }
        });
        let end = if prefix.is_empty() {
            "\u{10ffff}".into()
        } else {
            format!("{}0", prefix.trim_end_matches('/'))
        };
        let excluded = serde_json::to_string(
            &declarations
                .iter()
                .map(|node| &node.path)
                .collect::<Vec<_>>(),
        )?;
        let mut stmt = db.prepare(
            "WITH paths AS (SELECT substr(path,?4+1) AS tail FROM workflow_nodes WHERE operation=?1 AND path>=?2 AND path<?3 AND rowid<=?5),
             children AS (SELECT DISTINCT ?2 || CASE WHEN instr(tail,'/')=0 THEN tail ELSE substr(tail,1,instr(tail,'/')-1) END AS child FROM paths WHERE tail<>'' AND substr(tail,1,1)<>'!')
             SELECT child FROM children WHERE child>?6 AND child NOT IN (SELECT value FROM json_each(?7)) ORDER BY child LIMIT ?8",
        )?;
        let candidates = stmt
            .query_map(
                params![
                    operation,
                    prefix,
                    end,
                    prefix.chars().count(),
                    cursor.head,
                    cursor.key,
                    excluded,
                    page.limit + 1
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        for path in candidates {
            let declared = definition
                .as_ref()
                .and_then(|value| workflow_definition::find(value, &path));
            if rows.len() == page.limit
                || !push(
                    &mut rows,
                    project(db, &workflow, &path, declared.as_ref())?,
                    &mut used,
                    page,
                )?
            {
                more = true;
                break;
            }
            cursor.key = path;
        }
    }
    let result = WorkflowOverview {
        workflow,
        container: container.map(str::to_owned),
        nodes: finish(rows, &cursor, more)?,
        definition_available: definition.is_some(),
        definition_error,
    };
    ensure!(
        serde_json::to_vec(&result)?.len() <= page.max_bytes,
        "workflow overview exceeds requested byte budget"
    );
    Ok(result)
}

pub(super) fn synthetic_specification(
    db: &Connection,
    operation: &OperationId,
    path: &str,
) -> anyhow::Result<Value> {
    valid_path(path)?;
    if let Ok(definition) = workflow_text::definition(db, operation) {
        if let Some(declared) = workflow_definition::find(&definition, path) {
            return Ok(declared.specification);
        }
    }
    ensure!(
        descendant_state(db, operation, path)?.is_some(),
        "unknown workflow node"
    );
    Ok(serde_json::json!({"execution_group":path}))
}

fn details(
    db: &Connection,
    operation: &OperationId,
    path: &str,
) -> anyhow::Result<WorkflowNodeDetails> {
    valid_path(path)?;
    let workflow = summary(db, operation)?;
    let definition = workflow_text::definition(db, operation).ok();
    let declared = definition
        .as_ref()
        .and_then(|value| workflow_definition::find(value, path));
    let node = project(db, &workflow, path, declared.as_ref())?;
    let stored_spec: Option<(usize,Vec<u8>)> = db.query_row(
        "SELECT length(CAST(specification AS BLOB)),substr(CAST(specification AS BLOB),1,1024) FROM workflow_nodes WHERE operation=?1 AND path=?2",
        params![operation,path], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let (specification_preview, specification_truncated) =
        if let Some((length, mut bytes)) = stored_spec {
            if let Err(error) = std::str::from_utf8(&bytes) {
                bytes.truncate(error.valid_up_to());
            }
            let preview = String::from_utf8(bytes)?;
            let truncated = preview.len() < length;
            (preview, truncated)
        } else {
            let full = serde_json::to_string(&synthetic_specification(db, operation, path)?)?;
            let mut end = full.len().min(1024);
            while !full.is_char_boundary(end) {
                end -= 1;
            }
            (full[..end].to_owned(), end < full.len())
        };
    let mut stmt = db.prepare("SELECT dependency FROM workflow_dependencies WHERE operation=?1 AND node=?2 ORDER BY dependency LIMIT 101")?;
    let mut paths = stmt
        .query_map(params![operation, path], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(dependency) = declared.as_ref().and_then(|node| node.dependency.clone()) {
        if !paths.contains(&dependency) {
            paths.insert(0, dependency);
        }
    }
    let mut dependencies_truncated = paths.len() > WORKFLOW_MAX_ROWS;
    let mut dependencies = Vec::new();
    let mut blockers = Vec::new();
    let mut used = 8192;
    for dependency in paths.into_iter().take(WORKFLOW_MAX_ROWS) {
        let declaration = definition
            .as_ref()
            .and_then(|value| workflow_definition::find(value, &dependency));
        let state = project(db, &workflow, &dependency, declaration.as_ref())
            .map(|node| node.state)
            .unwrap_or(WorkflowState::Unknown);
        let item = WorkflowDependency {
            path: dependency.clone(),
            state,
        };
        used += serde_json::to_vec(&item)?.len() * 2 + 256;
        if used > WORKFLOW_MAX_PAGE_BYTES {
            dependencies_truncated = true;
            break;
        }
        if state != WorkflowState::Completed {
            blockers.push(WorkflowBlocker {
                state,
                path: Some(dependency),
                thread: None,
                message: "Dependency has not completed successfully.".into(),
            });
        }
        dependencies.push(item);
    }
    if blockers.len() < WORKFLOW_MAX_ROWS
        && matches!(
            node.state,
            WorkflowState::WaitingCapacity
                | WorkflowState::WaitingPermission
                | WorkflowState::WaitingHuman
                | WorkflowState::Unknown
                | WorkflowState::Stopping
                | WorkflowState::Blocked
        )
    {
        blockers.push(WorkflowBlocker {
            state: node.state,
            path: None,
            thread: node.child.clone().or(Some(workflow.owner)),
            message: match node.state {
                WorkflowState::WaitingCapacity => "Waiting for workflow or global child capacity.",
                WorkflowState::WaitingPermission => {
                    "Waiting for permission in the owning conversation."
                }
                WorkflowState::WaitingHuman => "Waiting for human input in the child conversation.",
                WorkflowState::Unknown => {
                    "Execution is uncertain; inspection does not replay work."
                }
                WorkflowState::Stopping => "Cancellation is waiting for owned work to settle.",
                _ => "A prerequisite or workflow failure prevents admission.",
            }
            .into(),
        });
    }
    let mut artifacts = Vec::new();
    if let Some(artifact) = &node.result {
        artifacts.push(WorkflowArtifactLink {
            artifact: artifact.clone(),
            kind: WorkflowArtifactKind::NodeResult,
        });
    }
    if let Some(child) = &node.child {
        let report: Option<String> = db
            .query_row("SELECT outcome FROM agents WHERE thread=?1", [child], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        if let Some(artifact) = report {
            artifacts.push(WorkflowArtifactLink {
                artifact: ArtifactId(artifact),
                kind: WorkflowArtifactKind::ChildReport,
            });
        }
    }
    let result = WorkflowNodeDetails {
        operation: operation.clone(),
        node,
        specification_preview,
        specification_truncated,
        dependencies,
        dependencies_truncated,
        blockers,
        artifacts,
    };
    ensure!(
        serde_json::to_vec(&result)?.len() <= WORKFLOW_MAX_PAGE_BYTES,
        "node detail exceeds byte bound"
    );
    Ok(result)
}

fn iterations(
    db: &Connection,
    session: &SessionId,
    operation: &OperationId,
    path: &str,
    page: &WorkflowPageRequest,
) -> anyhow::Result<WorkflowPage<WorkflowIteration>> {
    valid_path(path)?;
    let mut cursor = cursor(
        db,
        session,
        Some(operation),
        "iterations",
        Some(path),
        page,
        "workflow_iterations",
    )?;
    let mut stmt = db.prepare("SELECT iteration,result FROM workflow_iterations WHERE operation=?1 AND node=?2 AND iteration>=?3 AND rowid<=?4 ORDER BY iteration LIMIT ?5")?;
    let candidates = stmt
        .query_map(
            params![
                operation,
                path,
                cursor.position,
                cursor.head,
                page.limit + 1
            ],
            |r| Ok((r.get::<_, usize>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    let mut used = 2048;
    let mut more = false;
    for (iteration, result) in candidates {
        if rows.len() == page.limit {
            more = true;
            break;
        }
        let result = ArtifactId(result);
        let last_completed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM workflow_nodes WHERE operation=?1 AND path=?2 AND iteration=?3 AND state IN ('completed','failed'))",params![operation,path,iteration],|r|r.get(0))?;
        let termination = if path == "find" {
            let final_result: Option<String> = db.query_row("SELECT w.result FROM workflows w WHERE w.operation=?1 AND w.state IN ('completed','failed') AND ?2=(SELECT MAX(iteration) FROM workflow_iterations WHERE operation=?1 AND node='find')",params![operation,iteration],|r|r.get(0)).optional()?.flatten();
            final_result
                .map(|id| workflow_text::metadata(db, &ArtifactId(id)).map(|meta| meta.0))
                .transpose()?
                .flatten()
        } else if last_completed {
            workflow_text::metadata(db, &result)?.0
        } else {
            None
        };
        if !push(
            &mut rows,
            WorkflowIteration {
                path: path.into(),
                iteration,
                result,
                termination,
            },
            &mut used,
            page,
        )? {
            more = true;
            break;
        }
        cursor.position = iteration as i64 + 1;
    }
    finish(rows, &cursor, more)
}

fn agents(
    db: &Connection,
    session: &SessionId,
    operation: &OperationId,
    page: &WorkflowPageRequest,
) -> anyhow::Result<WorkflowPage<WorkflowAgent>> {
    let mut cursor = cursor(db, session, Some(operation), "agents", None, page, "agents")?;
    let mut stmt = db.prepare(
        "SELECT a.rowid,a.thread,substr(t.name,1,128),(SELECT n.path FROM workflow_nodes n WHERE n.operation=?1 AND n.child=a.thread ORDER BY n.path LIMIT 1),a.outcome
         FROM agents a JOIN threads t ON t.id=a.thread WHERE t.session=?2 AND (a.operation=?1 OR EXISTS(SELECT 1 FROM workflow_nodes n WHERE n.operation=?1 AND n.child=a.thread))
         AND a.rowid>?3 AND a.rowid<=?4 ORDER BY a.rowid LIMIT ?5",
    )?;
    let candidates = stmt
        .query_map(
            params![
                operation,
                session,
                cursor.position,
                cursor.head,
                page.limit + 1
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = Vec::new();
    let mut used = 2048;
    let mut more = false;
    for (ordinal, thread, name, node, result) in candidates {
        if rows.len() == page.limit {
            more = true;
            break;
        }
        let thread = ThreadId(thread);
        let recorded: Option<String> = db.query_row("SELECT state FROM workflow_nodes WHERE operation=?1 AND child=?2 AND state IN ('completed','failed','cancelled','unknown') ORDER BY path LIMIT 1",params![operation,thread],|r|r.get(0)).optional()?;
        let state = match recorded {
            Some(recorded) => raw_state(&recorded),
            None => child_state(db, &thread)?,
        };
        if !push(
            &mut rows,
            WorkflowAgent {
                thread,
                name,
                state,
                node,
                result: result.map(ArtifactId),
            },
            &mut used,
            page,
        )? {
            more = true;
            break;
        }
        cursor.position = ordinal;
    }
    finish(rows, &cursor, more)
}
