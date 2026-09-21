use super::*;

impl Agent {
    pub(super) async fn stream_completion(
        &mut self,
        max_tokens_override: Option<u32>,
    ) -> anyhow::Result<StreamOutcome> {
        let mut retries = 0u32;
        let mut recovered_overflow = false;
        let mut partial_text = String::new();
        loop {
            if self.is_cancelled() {
                return Ok(StreamOutcome::Interrupted(partial_text));
            }
            if !self.refresh_system().await? {
                return Ok(StreamOutcome::Interrupted(partial_text));
            }
            let opts = GenerateOptions {
                system: self.request_system.clone(),
                messages: self.history.clone(),
                tools: self.cfg.tools.specs(),
                cache: true,
                reasoning: self.reasoning,
                // The server chooses the cap except during truncation recovery.
                max_tokens: max_tokens_override,
                ..Default::default()
            };
            let Some(started) = until_cancelled(
                &self.cancel,
                self.cfg.parent_cancel.as_ref(),
                self.cfg.provider.stream(opts),
            )
            .await
            else {
                return Ok(StreamOutcome::Interrupted(partial_text));
            };
            let mut rx = match started {
                Ok(rx) => rx,
                Err(e) => match classify_error(&format!("{e:#}")) {
                    ProviderErrorKind::Overflow if !recovered_overflow => {
                        recovered_overflow = true;
                        match self.compact_now().await {
                            Ok(true) => continue,
                            Ok(false) => return Ok(StreamOutcome::Interrupted(partial_text)),
                            Err(error) => {
                                if !partial_text.is_empty() {
                                    self.record_summary(&partial_text);
                                }
                                return Err(error);
                            }
                        }
                    }
                    ProviderErrorKind::Transient if retries < MAX_RETRIES => {
                        retries += 1;
                        self.cfg.bus.emit(AgentEvent::StreamRetry {
                            agent_id: self.id.clone(),
                            attempt: retries,
                            max_attempts: MAX_RETRIES,
                        });
                        self.backoff().await;
                        continue;
                    }
                    _ => {
                        if !partial_text.is_empty() {
                            self.record_summary(&partial_text);
                        }
                        if retries == MAX_RETRIES && is_transient_error(&format!("{e:#}")) {
                            return Err(crate::providers::provider::retries_exhausted(e));
                        }
                        return Err(e);
                    }
                },
            };

            partial_text.clear();
            let mut completion = None;
            let mut stream_error = None;
            loop {
                // Poll cancellation even while the provider is silent.
                let evt =
                    match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                        .await
                    {
                        Ok(Some(evt)) => evt,
                        Ok(None) => break,
                        Err(_) => {
                            if self.is_cancelled() {
                                break;
                            }
                            continue;
                        }
                    };
                match evt {
                    StreamEvent::TextDelta { text } => {
                        partial_text.push_str(&text);
                        self.cfg.bus.emit(AgentEvent::TextDelta {
                            agent_id: self.id.clone(),
                            text,
                        });
                    }
                    StreamEvent::MessageStop { completion: done } => {
                        completion = Some(done);
                        break;
                    }
                    StreamEvent::Error { message } => {
                        stream_error = Some(message);
                        break;
                    }
                    _ => {}
                }
                if self.is_cancelled() {
                    break;
                }
            }

            drop(rx);

            if self.is_cancelled() && completion.is_none() {
                return Ok(StreamOutcome::Interrupted(partial_text));
            }

            if completion.is_none() && stream_error.is_none() {
                stream_error = Some("stream ended without completion".to_string());
            }
            if let Some(message) = stream_error {
                match classify_error(&message) {
                    ProviderErrorKind::Overflow if !recovered_overflow => {
                        recovered_overflow = true;
                        match self.compact_now().await {
                            Ok(true) => continue,
                            Ok(false) => return Ok(StreamOutcome::Interrupted(partial_text)),
                            Err(error) => {
                                if !partial_text.is_empty() {
                                    self.record_summary(&partial_text);
                                }
                                return Err(error);
                            }
                        }
                    }
                    ProviderErrorKind::Transient if retries < MAX_RETRIES => {
                        retries += 1;
                        self.cfg.bus.emit(AgentEvent::StreamRetry {
                            agent_id: self.id.clone(),
                            attempt: retries,
                            max_attempts: MAX_RETRIES,
                        });
                        self.backoff().await;
                        continue;
                    }
                    _ => {
                        if !partial_text.is_empty() {
                            self.record_summary(&partial_text);
                        }
                        let error = anyhow::anyhow!("{}", message);
                        if retries == MAX_RETRIES && is_transient_error(&message) {
                            return Err(crate::providers::provider::retries_exhausted(error));
                        }
                        return Err(error);
                    }
                }
            }

            return Ok(match completion {
                Some(c) => StreamOutcome::Completed(c),
                None => {
                    if !partial_text.is_empty() {
                        self.record_summary(&partial_text);
                    }
                    return Err(anyhow::anyhow!("stream ended without completion"));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::agent::{completion, config, text, ScriptProvider, Step};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use std::time::Duration;

    struct FailingProvider {
        error: &'static str,
        failures: usize,
        midstream: bool,
        calls: Mutex<Vec<tokio::time::Instant>>,
    }

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            "retry-test"
        }
        fn model(&self) -> &str {
            "retry-test"
        }
        async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
            assert!(opts.tools.is_empty());
            let mut calls = self.calls.lock().unwrap();
            calls.push(tokio::time::Instant::now());
            if calls.len() <= self.failures {
                anyhow::bail!(self.error);
            }
            Ok(completion(vec![text("recovered")]))
        }
        async fn stream(
            &self,
            _: GenerateOptions,
        ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>> {
            let fail = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(tokio::time::Instant::now());
                calls.len() <= self.failures
            };
            if fail && !self.midstream {
                return Err(anyhow::anyhow!(self.error).context("provider startup"));
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            if fail {
                tx.send(StreamEvent::TextDelta {
                    text: "discarded".into(),
                })
                .unwrap();
                tx.send(StreamEvent::Error {
                    message: self.error.into(),
                })
                .unwrap();
            } else {
                tx.send(StreamEvent::MessageStop {
                    completion: completion(vec![text("recovered")]),
                })
                .unwrap();
            }
            Ok(rx)
        }
    }

    fn failing_provider(
        error: &'static str,
        failures: usize,
        midstream: bool,
    ) -> Arc<FailingProvider> {
        Arc::new(FailingProvider {
            error,
            failures,
            midstream,
            calls: Mutex::new(Vec::new()),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn transport_failures_recover_at_startup_and_midstream_after_ten_seconds() {
        for midstream in [false, true] {
            for error in [
                "error sending request for url (https://provider.test)",
                "error decoding response body",
                "provider startup deadline exceeded",
                "SSE idle deadline exceeded",
                "responses: server_error: Internal server error",
                "HTTP 429: retry in 400 milliseconds",
            ] {
                let provider = failing_provider(error, 1, midstream);
                let mut agent = Agent::new(config(provider.clone()));
                let events = Arc::new(Mutex::new(Vec::new()));
                let captured = events.clone();
                agent.bus().on(Arc::new(move |event| {
                    captured.lock().unwrap().push(event.clone())
                }));
                let result = agent.run_with_budget("go", 1).await.unwrap();
                assert_eq!(result.text, "recovered");
                assert_eq!(agent.messages().len(), 2);
                assert_eq!(agent.messages()[1].text(), "recovered");
                let calls = provider.calls.lock().unwrap();
                assert_eq!(calls.len(), 2, "{error}");
                assert_eq!(calls[1] - calls[0], Duration::from_secs(10));
                assert_eq!(
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| matches!(
                            event,
                            AgentEvent::StreamRetry {
                                attempt: 1,
                                max_attempts: 10,
                                ..
                            }
                        ))
                        .count(),
                    1
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_exhaust_after_eleven_requests_with_fixed_delays() {
        let provider = failing_provider(
            "error sending request for url (https://provider.test)",
            11,
            false,
        );
        let mut agent = Agent::new(config(provider.clone()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        agent.bus().on(Arc::new(move |event| {
            if let AgentEvent::StreamRetry {
                attempt,
                max_attempts,
                ..
            } = event
            {
                captured.lock().unwrap().push((*attempt, *max_attempts));
            }
        }));
        let error = agent.stream_completion(None).await.err().unwrap();
        assert!(error.to_string().contains("failed after 10 retries"));
        assert!(error.to_string().contains("error sending request for url"));
        let calls = provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 11);
        for interval in calls.windows(2) {
            assert_eq!(interval[1] - interval[0], Duration::from_secs(10));
        }
        assert_eq!(
            *events.lock().unwrap(),
            (1..=10).map(|n| (n, 10)).collect::<Vec<_>>()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn turn_limit_summary_retries_are_visible_and_exhaustion_is_recorded() {
        for failures in [1, 11] {
            let provider = failing_provider("error decoding response body", failures, false);
            let mut agent = Agent::new(config(provider.clone()));
            agent.load_history(vec![Message::user_text("go")]);
            let progress = Arc::new(Mutex::new(Vec::new()));
            let captured = progress.clone();
            agent.bus().on(Arc::new(move |event| {
                if let AgentEvent::StreamRetry {
                    attempt,
                    max_attempts,
                    ..
                } = event
                {
                    captured.lock().unwrap().push((*attempt, *max_attempts));
                }
            }));
            let mut usage = Usage::default();
            let summary = agent.wind_down(1, &mut usage).await;
            let retries = failures.min(10);
            assert_eq!(provider.calls.lock().unwrap().len(), retries + 1);
            assert_eq!(
                *progress.lock().unwrap(),
                (1..=retries as u32).map(|n| (n, 10)).collect::<Vec<_>>()
            );
            if failures == 1 {
                assert_eq!(summary, "[turn limit reached] recovered");
            } else {
                assert!(summary.contains("failed after 10 retries"));
                assert!(summary.contains("error decoding response body"));
            }
            assert_eq!(agent.messages().last().unwrap().text(), summary);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_provider_failures_do_not_retry() {
        for error in [
            "HTTP 401: invalid API key; connection error",
            "HTTP 400: invalid schema; 500 tokens",
            "error sending request for url: invalid peer certificate: UnknownIssuer",
            "openai stream ended: malformed SSE JSON: expected value at line 1 column 1",
            "openai: incomplete or malformed arguments for bash: EOF while parsing an object at line 1 column 500",
        ] {
            let provider = failing_provider(error, 1, false);
            let mut agent = Agent::new(config(provider.clone()));
            let start = tokio::time::Instant::now();
            let result = agent.stream_completion(None).await;
            assert!(result.is_err(), "{error}");
            assert_eq!(provider.calls.lock().unwrap().len(), 1);
            assert_eq!(tokio::time::Instant::now(), start);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_the_retry_delay_without_another_request() {
        for cancel_parent in [false, true] {
            let provider = failing_provider("error decoding response body", 1, true);
            let parent = Arc::new(AtomicBool::new(false));
            let mut cfg = config(provider.clone());
            cfg.parent_cancel = Some(parent.clone());
            let mut agent = Agent::new(cfg);
            let cancel = agent.cancel_handle();
            let retry_started = Arc::new(tokio::sync::Notify::new());
            let notified = retry_started.clone();
            agent.bus().on(Arc::new(move |event| {
                if matches!(event, AgentEvent::StreamRetry { .. }) {
                    notified.notify_one();
                }
            }));
            let start = tokio::time::Instant::now();
            let (result, ()) = tokio::join!(agent.stream_completion(None), async {
                retry_started.notified().await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cancel_parent {
                    parent.store(true, Ordering::Relaxed);
                } else {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
            assert!(
                matches!(result.unwrap(), StreamOutcome::Interrupted(partial) if partial == "discarded")
            );
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(provider.calls.lock().unwrap().len(), 1);
            assert!(agent.messages().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_retry_startup_returns_last_partial_without_committing_it() {
        for cancel_parent in [false, true] {
            let provider = Arc::new(ScriptProvider::new(vec![
                Step::Events(vec![
                    StreamEvent::TextDelta {
                        text: "partial ".into(),
                    },
                    StreamEvent::TextDelta {
                        text: "findings".into(),
                    },
                    StreamEvent::Error {
                        message: "HTTP 503".into(),
                    },
                ]),
                Step::Pending,
            ]));
            let parent = Arc::new(AtomicBool::new(false));
            let mut cfg = config(provider.clone());
            cfg.parent_cancel = Some(parent.clone());
            let mut agent = Agent::new(cfg);
            agent.skip_retry_backoff = true;
            agent.load_history(vec![Message::user_text("go")]);
            let cancel = agent.cancel_handle();
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(agent.stream_completion(None), async {
                    provider.started.notified().await;
                    if cancel_parent {
                        parent.store(true, Ordering::Relaxed);
                    } else {
                        cancel.store(true, Ordering::Relaxed);
                    }
                })
            })
            .await
            .expect("retry startup must be interruptible");
            match outcome.unwrap() {
                StreamOutcome::Interrupted(partial) => assert_eq!(partial, "partial findings"),
                StreamOutcome::Completed(_) => panic!("cancelled startup cannot complete"),
            }
            for history in [agent.working_messages(), agent.messages()] {
                assert_eq!(history.len(), 1);
                assert_eq!(history[0].role, Role::User);
                assert_eq!(history[0].text(), "go");
            }
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_eq!(request.len(), 1);
                assert_eq!(request[0].text(), "go");
            }
            provider.assert_exhausted();
        }
    }

    #[tokio::test]
    async fn completed_message_is_authoritative_and_deltas_remain_in_event_order() {
        let reply = completion(vec![text("authoritative answer")]);
        let provider = Arc::new(ScriptProvider::new(vec![Step::Events(vec![
            StreamEvent::TextDelta {
                text: "preview ".into(),
            },
            StreamEvent::TextDelta {
                text: "only".into(),
            },
            StreamEvent::MessageStop {
                completion: reply.clone(),
            },
            StreamEvent::Error {
                message: "must not be consumed after completion".into(),
            },
        ])]));
        let mut agent = Agent::new(config(provider.clone()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        agent.bus().on(Arc::new(move |event| {
            captured.lock().unwrap().push(event.clone())
        }));
        match agent.stream_completion(Some(8192)).await.unwrap() {
            StreamOutcome::Completed(actual) => {
                assert_eq!(actual.message.text(), "authoritative answer");
                assert_eq!(actual.stop_reason, reply.stop_reason);
                assert_eq!(
                    serde_json::to_value(actual.usage).unwrap(),
                    serde_json::to_value(reply.usage).unwrap()
                );
            }
            StreamOutcome::Interrupted(_) => panic!("complete stream was interrupted"),
        }
        assert!(agent.messages().is_empty());
        assert!(agent.working_messages().is_empty());
        assert_eq!(*provider.caps.lock().unwrap(), vec![Some(8192)]);
        let events = events.lock().unwrap();
        assert!(matches!(events.as_slice(), [
            AgentEvent::TextDelta { agent_id: first_id, text: first },
            AgentEvent::TextDelta { agent_id: second_id, text: second },
        ] if first_id == "root" && second_id == "root" && first == "preview " && second == "only"));
        provider.assert_exhausted();
    }
}
