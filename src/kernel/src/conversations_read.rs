use super::*;

pub(super) fn read(
    db: &Connection,
    caller: &ThreadId,
    token: &str,
    request: Read,
) -> anyhow::Result<String> {
    let (session, requested_thread, budget) = match &request {
        Read::Transcript {
            session_id,
            thread_id,
            limit_bytes,
            ..
        }
        | Read::Entry {
            session_id,
            thread_id,
            limit_bytes,
            ..
        }
        | Read::Threads {
            session_id,
            thread_id,
            limit_bytes,
            ..
        } => (session_id, thread_id.as_ref(), *limit_bytes),
    };
    valid_id(&session.0)?;
    ensure!(
        (1024..=MAX_OUTPUT).contains(&budget),
        "limit_bytes must be 1024..24576"
    );
    let root: ThreadId = db
        .query_row(
            "SELECT id FROM threads WHERE session=?1 AND parent_run IS NULL",
            [session],
            |r| Ok(ThreadId(r.get(0)?)),
        )
        .optional()?
        .context("conversation session not found")?;
    let target = requested_thread.unwrap_or(&root);
    valid_id(&target.0)?;
    let owned: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?1 AND session=?2)",
        params![target, session],
        |r| r.get(0),
    )?;
    ensure!(
        owned,
        "thread does not belong to the requested conversation"
    );
    match &request {
        Read::Entry {
            sequence, offset, ..
        } => {
            ensure!(*sequence > 0, "sequence must be positive");
            let mut remaining = SCAN_BYTES;
            let projected = projection::project(db, target, *sequence, &mut remaining)?
                .context("entry exceeds source budget")?;
            let mut notes = Limitations::new();
            note(&mut notes, projected.limitation);
            crate::history::text_response(
                db,
                target,
                projected.source,
                *offset,
                budget,
                json!({"view":"entry","session_id":session,"sequence":sequence,"limitations":limitations(&notes)}),
            )
        }
        Read::Transcript {
            limit,
            direction,
            cursor,
            around_sequence,
            ..
        } => {
            ensure!((1..=100).contains(limit), "transcript limit must be 1..100");
            if let Some(around) = around_sequence {
                ensure!(
                    *around > 0 && *direction == Direction::OldestFirst,
                    "around_sequence requires a positive sequence and oldest_first direction"
                );
                let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM entries WHERE seq=?1 AND thread=?2 AND kind IN ('assistant','submission','coordination'))", params![around,target], |r| r.get(0))?;
                ensure!(
                    exists,
                    "around_sequence is not a visible entry in the target thread"
                );
            }
            let head: i64 = db.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM entries WHERE thread=?1",
                [target],
                |r| r.get(0),
            )?;
            let binding = json!(["transcript", session, target, direction, around_sequence]);
            let mut state = Cursor::load(cursor.as_deref(), token, caller, binding, head, 0)?;
            ensure!(
                state.titles_done && state.titles_position == 0,
                "invalid transcript cursor"
            );
            if cursor.is_none() {
                state.position = if *direction == Direction::NewestFirst {
                    head.saturating_add(1)
                } else {
                    0
                };
            } else {
                ensure!(
                    state.position > 0 && state.position <= state.head,
                    "invalid transcript cursor position"
                );
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries WHERE thread=?1 AND seq=?2)",
                    params![target, state.position],
                    |r| r.get(0),
                )?;
                ensure!(exists, "transcript cursor does not belong to target thread");
            }
            transcript(db, session, target, *limit, *direction, state, budget)
        }
        Read::Threads { limit, cursor, .. } => {
            ensure!((1..=100).contains(limit), "threads limit must be 1..100");
            let head: i64 = db.query_row(
                "SELECT COALESCE(MAX(rowid),0) FROM threads WHERE session=?1",
                [session],
                |r| r.get(0),
            )?;
            let binding = json!(["threads", session, target]);
            let state = Cursor::load(cursor.as_deref(), token, caller, binding, head, 0)?;
            ensure!(
                state.titles_done && state.titles_position == 0,
                "invalid threads cursor"
            );
            if state.position > 0 {
                let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM threads WHERE session=?1 AND rowid=?2 AND rowid<=?3)", params![session,state.position,state.head], |r| r.get(0))?;
                ensure!(exists, "threads cursor does not belong to target session");
            }
            threads(db, session, target, *limit, state, budget)
        }
    }
}

fn preview(
    db: &Connection,
    sequence: i64,
    kind: &str,
    projected: &Projection,
    budget: usize,
) -> anyhow::Result<Value> {
    let source = &projected.source;
    let total = source.total(db)?;
    let text = source.page(db, 0, (budget.saturating_sub(1000) / 6).clamp(4, 160))?;
    let mut item = json!({"sequence":sequence,"kind":kind,"text":text,"source":source.name,"preview_only":source.preview_only,"original_complete":source.original_complete,"text_truncated":text.len()<total,"total_bytes":total});
    if let Some(routing) = &source.routing {
        item["routing"] = routing.clone();
    }
    if let Some(code) = projected.limitation {
        item["limitation"] = json!(code);
    }
    Ok(item)
}

fn around_start(
    db: &Connection,
    target: &ThreadId,
    around: i64,
    wanted: usize,
) -> anyhow::Result<(i64, usize, bool)> {
    if wanted == 0 {
        return Ok((around - 1, 0, false));
    }
    let cap = (MAX_CANDIDATES - 1) / 2;
    let mut statement = db.prepare("SELECT seq,kind,length(text)>0 FROM entries WHERE thread=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3")?;
    let mut rows = statement.query(params![target, around, cap])?;
    let mut examined = 0;
    let mut visible = 0;
    let mut oldest = around;
    let mut position = around - 1;
    while let Some(row) = rows.next()? {
        examined += 1;
        oldest = row.get(0)?;
        let kind: String = row.get(1)?;
        if projection::visible(&kind) && row.get::<_, bool>(2)? {
            position = oldest - 1;
            visible += 1;
            if visible == wanted {
                break;
            }
        }
    }
    let limited = examined == cap
        && visible < wanted
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE thread=?1 AND seq<?2)",
            params![target, oldest],
            |row| row.get::<_, bool>(0),
        )?;
    Ok((position, examined, limited))
}

fn transcript(
    db: &Connection,
    session: &SessionId,
    target: &ThreadId,
    limit: usize,
    direction: Direction,
    mut cursor: Cursor,
    budget: usize,
) -> anyhow::Result<String> {
    let sql = if direction == Direction::OldestFirst {
        "SELECT seq,kind FROM entries WHERE thread=?1 AND seq<=?2 AND seq>?3 ORDER BY seq LIMIT 1"
    } else {
        "SELECT seq,kind FROM entries WHERE thread=?1 AND seq<=?2 AND seq<?3 ORDER BY seq DESC LIMIT 1"
    };
    let mut entries = Vec::new();
    let mut examined = 0;
    let mut remaining = SCAN_BYTES;
    let mut notes = Limitations::new();
    if cursor.position == 0 {
        if let Some(around) = cursor.binding[4].as_i64() {
            let (position, scanned, limited) = around_start(db, target, around, limit / 2)?;
            cursor.position = position;
            examined = scanned;
            note(&mut notes, limited.then_some("preceding_context_limited"));
        }
    }
    let mut complete = false;
    while examined < MAX_CANDIDATES && remaining >= METADATA_RESERVE && entries.len() < limit {
        let row: Option<(i64, String)> = db
            .query_row(sql, params![target, cursor.head, cursor.position], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((sequence, kind)) = row else {
            complete = true;
            break;
        };
        examined += 1;
        if !projection::visible(&kind) {
            cursor.position = sequence;
            continue;
        }
        let Some(projected) = projection::project(db, target, sequence, &mut remaining)? else {
            break;
        };
        if projected.limitation.is_none() && projected.source.total(db)? == 0 {
            cursor.position = sequence;
            continue;
        }
        let item = preview(db, sequence, &kind, &projected, budget)?;
        let previous = cursor.position;
        cursor.position = sequence;
        entries.push(item);
        let mut proposed_notes = notes.clone();
        note(&mut proposed_notes, projected.limitation);
        let response = transcript_response(
            session,
            target,
            &entries,
            &cursor,
            false,
            examined,
            &proposed_notes,
        )?;
        if serde_json::to_string(&response)?.len() > budget {
            entries.pop();
            cursor.position = previous;
            ensure!(
                !entries.is_empty(),
                "page budget cannot fit transcript entry; increase limit_bytes"
            );
            break;
        }
        notes = proposed_notes;
    }
    if !complete {
        complete = db
            .query_row(sql, params![target, cursor.head, cursor.position], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_none();
    }
    let encoded = serde_json::to_string(&transcript_response(
        session, target, &entries, &cursor, complete, examined, &notes,
    )?)?;
    ensure!(
        encoded.len() <= budget,
        "page budget cannot fit transcript metadata"
    );
    Ok(encoded)
}

fn transcript_response(
    session: &SessionId,
    target: &ThreadId,
    entries: &[Value],
    cursor: &Cursor,
    complete: bool,
    examined: usize,
    notes: &Limitations,
) -> anyhow::Result<Value> {
    Ok(
        json!({"view":"transcript","session_id":session,"thread_id":target,"direction":cursor.binding[3],"initial_head":cursor.head,"entries":entries,"next_cursor":if complete { None } else { Some(cursor.encode()?) },"retention":"retained_entries","scan_complete":complete,"examined_entries":examined,"limitations":limitations(notes)}),
    )
}

fn threads(
    db: &Connection,
    session: &SessionId,
    target: &ThreadId,
    limit: usize,
    mut cursor: Cursor,
    budget: usize,
) -> anyhow::Result<String> {
    let mut items = Vec::new();
    let mut notes = Limitations::new();
    for _ in 0..limit {
        let row: Option<(i64, String, String, Option<String>, bool)> = db.query_row(
            "SELECT t.rowid,t.id,substr(t.name,1,80),parent.id,length(t.name)>80 FROM threads t LEFT JOIN runs r ON r.id=t.parent_run LEFT JOIN threads parent ON parent.id=r.thread AND parent.session=t.session WHERE t.session=?1 AND t.rowid>?2 AND t.rowid<=?3 ORDER BY t.rowid LIMIT 1",
            params![session,cursor.position,cursor.head], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
        ).optional()?;
        let Some((position, thread, name, parent, truncated)) = row else {
            break;
        };
        let previous = cursor.position;
        cursor.position = position;
        items.push(json!({"thread_id":thread,"parent_thread_id":parent,"name":name}));
        let response = json!({"view":"threads","session_id":session,"thread_id":target,"threads":items,"next_cursor":cursor.encode()?,"scan_complete":false,"limitations":limitations(&notes)});
        if serde_json::to_string(&response)?.len() > budget.saturating_sub(80) {
            items.pop();
            cursor.position = previous;
            ensure!(
                !items.is_empty(),
                "page budget cannot fit thread metadata; increase limit_bytes"
            );
            break;
        }
        note(&mut notes, truncated.then_some("metadata_truncated"));
    }
    let more: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads WHERE session=?1 AND rowid>?2 AND rowid<=?3)",
        params![session, cursor.position, cursor.head],
        |r| r.get(0),
    )?;
    let encoded = serde_json::to_string(
        &json!({"view":"threads","session_id":session,"thread_id":target,"threads":items,"next_cursor":if more { Some(cursor.encode()?) } else { None },"scan_complete":!more,"limitations":limitations(&notes)}),
    )?;
    ensure!(
        encoded.len() <= budget,
        "page budget cannot fit threads metadata"
    );
    Ok(encoded)
}
