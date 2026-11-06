//! Live dictation [`Session`]: manually-driven utterances → [`Event`] stream.
//!
//! The caller controls utterance boundaries explicitly with [`start`](Session::start)
//! and [`stop`](Session::stop) — this is the push-to-talk model (a hotkey down
//! starts capture, hotkey up ends it and triggers transcription). Utterance
//! detection from audio (VAD) is intentionally not built in: triggering is a
//! platform/UX concern the caller owns.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use crate::api::Voice;
use crate::audio::AudioCapture;
use crate::state::{DictationState, Event};

/// A command from the caller to the session worker.
enum Command {
    /// Begin capturing a new utterance.
    Start,
    /// End the current utterance, transcribe it, and emit a `Final` event.
    Stop,
    /// Shut the session down.
    Quit,
}

/// A running dictation session. Drive it with [`start`](Session::start) /
/// [`stop`](Session::stop); read results from [`events`](Session::events).
pub struct Session {
    events_rx: Receiver<Event>,
    cmd_tx: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Session {
    pub(crate) fn new(voice: Arc<Voice>) -> Result<Self> {
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let worker = std::thread::spawn(move || {
            if let Err(e) = run_loop(&voice, &events_tx, &cmd_rx) {
                let _ = events_tx.send(Event::Error {
                    message: e.to_string(),
                });
            }
        });

        Ok(Self {
            events_rx,
            cmd_tx,
            worker: Some(worker),
        })
    }

    /// Begin capturing a new utterance (e.g. on hotkey down).
    pub fn start(&self) {
        let _ = self.cmd_tx.send(Command::Start);
    }

    /// End the current utterance and transcribe it (e.g. on hotkey up).
    /// The result arrives as an [`Event::Final`] on the event stream.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    /// The event stream. Iterating blocks until the session is dropped.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events_rx.iter()
    }

    /// Non-blocking receiver, if you want to poll instead of iterate.
    pub fn receiver(&self) -> &Receiver<Event> {
        &self.events_rx
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Quit);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The capture loop: buffers audio while an utterance is active, transcribes on
/// `Stop`, and emits state/`Final` events.
fn run_loop(voice: &Voice, events: &Sender<Event>, cmd: &mpsc::Receiver<Command>) -> Result<()> {
    use std::time::Duration;

    let audio = AudioCapture::start()?;
    let mut buffer: Vec<f32> = Vec::new();
    let mut active = false;
    events
        .send(Event::State {
            state: DictationState::Idle,
        })
        .ok();

    loop {
        // Handle any pending commands (non-blocking).
        match cmd.try_recv() {
            Ok(Command::Start) => {
                active = true;
                buffer.clear();
                // Drop buffered pre-roll so we don't capture stale audio.
                while audio.samples.try_recv().is_ok() {}
                events
                    .send(Event::State {
                        state: DictationState::Listening,
                    })
                    .ok();
            }
            Ok(Command::Stop) => {
                if active {
                    active = false;
                    events
                        .send(Event::State {
                            state: DictationState::Transcribing,
                        })
                        .ok();
                    let text = voice.transcribe_locked(&buffer)?;
                    buffer.clear();
                    if !text.is_empty() {
                        events.send(Event::Final { text }).ok();
                    }
                    events
                        .send(Event::State {
                            state: DictationState::Idle,
                        })
                        .ok();
                }
            }
            Ok(Command::Quit) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // Drain available audio while active.
        if active {
            if let Ok(chunk) = audio.samples.recv_timeout(Duration::from_millis(50)) {
                buffer.extend_from_slice(&chunk);
            }
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
