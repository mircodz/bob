use crate::contracts::*;
use crate::model::*;
use crate::store::*;
use anyhow::ensure;
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;

pub struct SessionCatalog {
    store: Store,
    archive_token: String,
}
impl SessionCatalog {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let application: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            application == APPLICATION && matches!(schema, 8 | 9 | SCHEMA),
            "unsupported kernel database format; preserved without modification"
        );
        Ok(Self {
            store: Store::read_only(db),
            archive_token: uuid::Uuid::new_v4().to_string(),
        })
    }
    pub fn search_conversation(
        &self,
        caller: &ThreadId,
        input: &serde_json::Value,
    ) -> anyhow::Result<String> {
        crate::conversations::execute(
            &self.store.db,
            caller,
            &self.archive_token,
            "search_conversation",
            input,
        )
    }
    pub fn read_conversation(
        &self,
        caller: &ThreadId,
        input: &serde_json::Value,
    ) -> anyhow::Result<String> {
        crate::conversations::execute(
            &self.store.db,
            caller,
            &self.archive_token,
            "read_conversation",
            input,
        )
    }
    pub fn sessions(&self, query: SessionQuery) -> anyhow::Result<SessionPage> {
        sessions(&self.store.db, query)
    }
    pub fn models(&self, id: SessionId) -> anyhow::Result<Vec<String>> {
        let mut stmt=self.store.db.prepare("SELECT DISTINCT json_extract(profile,'$.model') FROM threads WHERE session=?1 ORDER BY 1")?;
        let models = stmt
            .query_map([id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(models)
    }
    pub fn view(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<View> {
        self.store.view(thread, before, limit, None)
    }
    pub fn transcript(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<View> {
        self.store.transcript(thread, before, limit, None)
    }
    pub fn transcript_after(
        &self,
        thread: Option<ThreadId>,
        after: i64,
        limit: usize,
    ) -> anyhow::Result<View> {
        self.store.transcript_after(thread, after, limit, None)
    }
    pub fn artifact(&self, id: ArtifactId, offset: usize, limit: usize) -> anyhow::Result<String> {
        self.store.artifact_page(&id, offset, limit)
    }
    pub fn entry_text(
        &self,
        thread: ThreadId,
        sequence: i64,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        self.store.entry_text(&thread, sequence, offset, limit)
    }
    pub fn tool_output(
        &self,
        operation: OperationId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        self.store.tool_output(&operation, offset, limit)
    }
    pub fn session(&self, id: SessionId) -> anyhow::Result<SessionSnapshot> {
        session(&self.store.db, &id)
    }
}

pub(crate) fn sessions(db: &Connection, query: SessionQuery) -> anyhow::Result<SessionPage> {
    let limit = if query.limit == 0 {
        50
    } else {
        query.limit.clamp(1, 200)
    };
    let title_search = if crate::titles::has_schema(db)? {
        " OR EXISTS(SELECT 1 FROM session_titles st WHERE st.session=s.id AND instr(lower(st.title),lower(?3))>0)"
    } else {
        ""
    };
    let mut stmt = db.prepare(&format!("SELECT s.updated,s.id,t.id,s.workspace,json_extract(t.profile,'$.model'),COALESCE((SELECT e.text FROM entries e WHERE e.thread=t.id AND e.kind='submission' ORDER BY e.seq DESC LIMIT 1),''),s.updated_at,(SELECT COUNT(*) FROM entries e WHERE e.thread=t.id AND e.kind IN ('submission','assistant')) FROM sessions s JOIN threads t ON t.session=s.id AND t.parent_run IS NULL WHERE s.updated<?1 AND (?2 IS NULL OR s.workspace=?2) AND (?3 IS NULL OR instr(lower(s.workspace),lower(?3))>0 OR EXISTS(SELECT 1 FROM entries e WHERE e.thread=t.id AND instr(lower(e.text),lower(?3))>0){title_search}) ORDER BY s.updated DESC LIMIT ?4"))?;
    let rows = stmt.query_map(
        params![
            query.before.unwrap_or(i64::MAX),
            query.workspace,
            query.search,
            limit + 1
        ],
        |r| {
            Ok(SessionSummary {
                ordinal: r.get(0)?,
                id: SessionId(r.get(1)?),
                thread: ThreadId(r.get(2)?),
                workspace: r.get(3)?,
                model: r.get(4)?,
                preview: r.get(5)?,
                updated_at: r.get(6)?,
                message_count: r.get(7)?,
                title: String::new(),
                human_previews: Vec::new(),
            })
        },
    )?;
    let mut sessions = rows.collect::<Result<Vec<_>, _>>()?;
    let more = sessions.len() > limit;
    sessions.truncate(limit);
    for session in &mut sessions {
        session.title = crate::titles::effective(db, &session.id, &session.thread)?.0;
        let mut stmt=db.prepare("SELECT kind,text FROM entries WHERE thread=?1 AND kind IN ('submission','assistant') AND trim(text)!='' ORDER BY seq DESC LIMIT 3")?;
        session.human_previews = stmt
            .query_map([&session.thread], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (kind, text) = row?;
                Ok(format!(
                    "{}: {}",
                    if kind == "submission" { "you" } else { "bob" },
                    one_line(&text, 80)
                ))
            })
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        session.human_previews.reverse();
    }
    Ok(SessionPage {
        next_before: more.then(|| sessions.last().unwrap().ordinal),
        sessions,
    })
}

fn one_line(text: &str, limit: usize) -> String {
    let text = text.trim().replace(['\r', '\n'], " ");
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text
    }
}

pub(crate) fn session(db: &Connection, id: &SessionId) -> anyhow::Result<SessionSnapshot> {
    let (thread,workspace,profile,profile_revision,epoch,policy,permissions,revision): (String,String,String,i64,i64,String,String,i64) = db.query_row("SELECT t.id,s.workspace,t.profile,t.profile_revision,c.epoch,s.policy,s.permissions,s.revision FROM sessions s JOIN threads t ON t.session=s.id AND t.parent_run IS NULL JOIN contexts c ON c.thread=t.id WHERE s.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
    let thread = ThreadId(thread);
    let (title, title_revision) = crate::titles::effective(db, id, &thread)?;
    Ok(SessionSnapshot {
        id: id.clone(),
        title,
        title_revision,
        thread,
        workspace,
        profile: decode(profile)?,
        profile_revision,
        context_epoch: epoch,
        policy: decode(policy)?,
        permissions: decode(permissions)?,
        policy_revision: revision,
    })
}

pub(crate) fn usage(db: &Connection, session: Option<&SessionId>) -> anyhow::Result<UsageSnapshot> {
    let mut snapshot = db.query_row("SELECT COUNT(*),COALESCE(SUM(u.compact),0),COALESCE(SUM(u.input),0),COALESCE(SUM(u.output),0),COALESCE(SUM(u.cache_write),0),COALESCE(SUM(u.cache_read),0) FROM usage u JOIN threads t ON t.id=u.thread WHERE (?1 IS NULL OR t.session=?1)",[session],|r|Ok(UsageSnapshot {requests:r.get(0)?,compaction_requests:r.get(1)?,usage:bob_types::Usage{input_tokens:r.get(2)?,output_tokens:r.get(3)?,cache_creation_input_tokens:r.get(4)?,cache_read_input_tokens:r.get(5)?}}))?;
    if crate::titles::has_schema(db)? {
        let extra = db.query_row(
            "SELECT COALESCE(SUM(dispatched),0),COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),COALESCE(SUM(cache_write),0),COALESCE(SUM(cache_read),0) FROM session_titles WHERE (?1 IS NULL OR session=?1)",
            [session],
            |r| Ok(UsageSnapshot {
                requests: r.get(0)?,
                compaction_requests: 0,
                usage: bob_types::Usage {
                    input_tokens: r.get(1)?,
                    output_tokens: r.get(2)?,
                    cache_creation_input_tokens: r.get(3)?,
                    cache_read_input_tokens: r.get(4)?,
                },
            }),
        )?;
        snapshot.requests += extra.requests;
        snapshot.usage.input_tokens += extra.usage.input_tokens;
        snapshot.usage.output_tokens += extra.usage.output_tokens;
        snapshot.usage.cache_creation_input_tokens += extra.usage.cache_creation_input_tokens;
        snapshot.usage.cache_read_input_tokens += extra.usage.cache_read_input_tokens;
    }
    Ok(snapshot)
}

pub(crate) fn thread_details(
    db: &Connection,
    session: &SessionId,
) -> anyhow::Result<Vec<ThreadDetails>> {
    let mut stmt=db.prepare("WITH RECURSIVE family(id,depth) AS (SELECT id,0 FROM threads WHERE session=?1 AND parent_run IS NULL UNION ALL SELECT t.id,f.depth+1 FROM threads t JOIN runs r ON r.id=t.parent_run JOIN family f ON f.id=r.thread WHERE t.session=?1) SELECT t.id,t.rowid,f.depth,COALESCE((SELECT substr(s.text,1,2048) FROM submissions s WHERE s.thread=t.id ORDER BY s.rowid LIMIT 1),''),o.id FROM threads t JOIN family f ON f.id=t.id LEFT JOIN agents a ON a.thread=t.id LEFT JOIN operations o ON o.id=a.operation AND o.name='workflow' ORDER BY t.rowid DESC LIMIT 1000")?;
    let details = stmt
        .query_map([session], |r| {
            Ok(ThreadDetails {
                thread: ThreadId(r.get(0)?),
                launch_sequence: r.get(1)?,
                depth: r.get(2)?,
                task: r.get(3)?,
                workflow_operation: r.get::<_, Option<String>>(4)?.map(OperationId),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(details)
}

pub(crate) fn record_usage(
    db: &Connection,
    operation: &OperationId,
    usage: bob_types::Usage,
) -> anyhow::Result<()> {
    db.execute("INSERT INTO usage(operation,thread,model,compact,input,output,cache_write,cache_read) SELECT id,thread,COALESCE(json_extract(input,'$.model'),name),name='compact',?2,?3,?4,?5 FROM operations WHERE id=?1",params![operation,usage.input_tokens,usage.output_tokens,usage.cache_creation_input_tokens,usage.cache_read_input_tokens])?;
    Ok(())
}
