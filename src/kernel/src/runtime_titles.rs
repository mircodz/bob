use super::*;
use crate::titles;
use bob_types::{ReasoningEffort, Usage};
use rusqlite::TransactionBehavior;
use std::sync::atomic::Ordering;

const DEADLINE: Duration = Duration::from_secs(18);

pub(super) struct TitleWorker {
    session: SessionId,
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TitleWorker {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}

pub(super) struct TitleFinished {
    session: SessionId,
    title: Option<String>,
    usage: Usage,
}

async fn generate_title(
    executor: Arc<dyn Executor>,
    request: RequestSpec,
    mut cancel: watch::Receiver<bool>,
    deadline: Duration,
) -> (Option<String>, Usage) {
    let (progress, invalid) = crate::ModelProgress::bounded_sink(titles::OUTPUT_BYTES);
    let generate =
        std::panic::AssertUnwindSafe(executor.generate(request, cancel.clone(), progress))
            .catch_unwind();
    let result = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => None,
        result = tokio::time::timeout(deadline, generate) => match result {
            Ok(Ok(Ok(completion))) => Some(completion),
            _ => None,
        },
    };
    match result {
        Some(completion) => {
            let title = if invalid.load(Ordering::Acquire) {
                None
            } else {
                titles::validated(&completion)
            };
            (title, completion.usage)
        }
        None => (None, Usage::default()),
    }
}

impl Actor {
    pub(super) fn schedule_title(&mut self) -> anyhow::Result<()> {
        if !self.options.auto_titles || self.title_worker.is_some() || self.workers.len() >= 16 {
            return Ok(());
        }
        let Some(selected) = self.selected_session.as_ref() else {
            return Ok(());
        };
        if !self
            .store
            .claim
            .as_ref()
            .is_some_and(|owner| owner.session() == selected)
        {
            return Ok(());
        }
        self.store.ensure_owner()?;
        let queued: Option<(String, String, String, String)> = self.store.db.query_row(
            "SELECT st.session,st.input,st.model,s.workspace FROM session_titles st JOIN sessions s ON s.id=st.session JOIN threads t ON t.id=st.thread JOIN contexts c ON c.thread=t.id WHERE st.state='queued' AND st.epoch=c.epoch AND st.profile_revision=t.profile_revision AND t.held=0 AND st.session=?1 ORDER BY st.rowid LIMIT 1",
            [selected],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        let Some((session, input, model, workspace)) = queued else {
            return Ok(());
        };
        let session = SessionId(session);
        let Some(model) = self
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .cloned()
        else {
            let tx = self
                .store
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE session_titles SET state='fallback',input='' WHERE session=?1 AND state='queued'",
                [&session],
            )?;
            tx.commit()?;
            return Ok(());
        };
        let request = RequestSpec {
            max_tokens: 128.min(self.model_capabilities(&model.id).max_output_tokens),
            model,
            workspace,
            system: titles::INSTRUCTION.into(),
            messages: vec![Message::user_text(titles::prefix(
                &input,
                titles::INPUT_BYTES,
            ))],
            tools: Vec::new(),
            reasoning: ReasoningEffort::Off,
        };
        let tx = self
            .store
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE session_titles SET state='running',input='',dispatched=1 WHERE session=?1 AND state='queued'", [&session])?;
        change(&tx, None, "session_metadata", &session.0)?;
        tx.commit()?;
        let (cancel, signal) = watch::channel(false);
        let executor = self.executor.clone();
        let done = self.title_tx.clone();
        let target = session.clone();
        let task = tokio::spawn(async move {
            let (title, usage) = generate_title(executor, request, signal, DEADLINE).await;
            let _ = done
                .send(TitleFinished {
                    session: target,
                    title,
                    usage,
                })
                .await;
        });
        self.title_worker = Some(TitleWorker {
            session,
            cancel,
            task,
        });
        Ok(())
    }

    pub(super) fn cancel_titles(&mut self, all: bool) -> anyhow::Result<()> {
        let Some(session) = self.selected_session.as_ref() else {
            return Ok(());
        };
        if !self
            .store
            .claim
            .as_ref()
            .is_some_and(|owner| owner.session() == session)
        {
            return Ok(());
        }
        self.store.ensure_owner()?;
        let pending: bool = self.store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_titles WHERE session=?1 AND state IN ('queued','running'))",
            [session], |r| r.get(0),
        )?;
        if !pending && self.title_worker.is_none() {
            return Ok(());
        }
        let tx = self
            .store
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE session_titles SET state='fallback',input='' WHERE session=?1 AND state IN ('queued','running') AND (?2 OR NOT EXISTS(SELECT 1 FROM threads t JOIN contexts c ON c.thread=t.id WHERE t.id=session_titles.thread AND t.session=session_titles.session AND t.held=0 AND c.epoch=session_titles.epoch AND t.profile_revision=session_titles.profile_revision))",
            params![session, all],
        )?;
        let cancel = if let Some(worker) = &self.title_worker {
            worker.session == *session && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_titles WHERE session=?1 AND state='running')",
                [&worker.session],
                |r| r.get::<_, bool>(0),
            )?
        } else {
            false
        };
        tx.commit()?;
        if cancel {
            if let Some(worker) = &self.title_worker {
                let _ = worker.cancel.send(true);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn finish_title(&mut self, finished: TitleFinished) -> anyhow::Result<()> {
        self.finish_title_ref(&finished)
    }

    pub(super) fn finish_title_ref(&mut self, finished: &TitleFinished) -> anyhow::Result<()> {
        let active = self.selected_session.as_ref() == Some(&finished.session)
            && self
                .store
                .claim
                .as_ref()
                .is_some_and(|owner| owner.session() == &finished.session)
            && self
                .title_worker
                .as_ref()
                .is_some_and(|worker| worker.session == finished.session);
        if !active {
            return Ok(());
        }
        self.store.ensure_owner()?;
        let tx = self
            .store
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let usage = &finished.usage;
        let usage_changed = tx.execute(
            "UPDATE session_titles SET input_tokens=?2,output_tokens=?3,cache_write=?4,cache_read=?5 WHERE session=?1 AND dispatched=1 AND (input_tokens!=?2 OR output_tokens!=?3 OR cache_write!=?4 OR cache_read!=?5)",
            params![finished.session, usage.input_tokens, usage.output_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens],
        )?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_titles st JOIN threads t ON t.id=st.thread JOIN contexts c ON c.thread=t.id WHERE st.session=?1 AND st.dispatched=1 AND st.state='running' AND st.epoch=c.epoch AND st.profile_revision=t.profile_revision AND st.session=t.session AND t.held=0)",
            [&finished.session],
            |r| r.get(0),
        )?;
        let title = finished.title.clone().filter(|_| valid);
        let title_changed = tx.execute(
            "UPDATE session_titles SET title=COALESCE(?2,title),state=CASE WHEN ?2 IS NULL THEN 'fallback' ELSE 'generated' END,input='' WHERE session=?1 AND dispatched=1 AND state='running'",
            params![finished.session, title],
        )?;
        if usage_changed != 0 || title_changed != 0 {
            change(&tx, None, "session_metadata", &finished.session.0)?;
        }
        tx.commit()?;
        drop(self.title_worker.take());
        Ok(())
    }
}

#[cfg(test)]
#[path = "title_tests.rs"]
mod tests;
