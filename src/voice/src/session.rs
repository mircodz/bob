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
    /// End the current utterance and discard its audio.
    Cancel,
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

    /// End the current utterance without transcribing it.
    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(Command::Cancel);
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

/// The capture loop: the microphone is open only while an utterance is active,
/// so the OS recording indicator turns off as soon as capture stops.
fn run_loop(voice: &Voice, events: &Sender<Event>, cmd: &mpsc::Receiver<Command>) -> Result<()> {
    use std::time::Duration;

    let mut audio: Option<AudioCapture> = None;
    let mut buffer: Vec<f32> = Vec::new();
    let state = |state| {
        events.send(Event::State { state }).ok();
    };
    state(DictationState::Idle);

    loop {
        let command = if audio.is_some() {
            cmd.try_recv()
        } else {
            cmd.recv().map_err(|_| mpsc::TryRecvError::Disconnected)
        };
        match command {
            Ok(Command::Start) => {
                if audio.is_none() {
                    buffer.clear();
                    match AudioCapture::start() {
                        Ok(capture) => {
                            audio = Some(capture);
                            state(DictationState::Listening);
                        }
                        Err(error) => {
                            events
                                .send(Event::Error {
                                    message: format!("microphone unavailable: {error}"),
                                })
                                .ok();
                            state(DictationState::Idle);
                        }
                    }
                }
            }
            Ok(Command::Stop) => {
                if let Some(capture) = audio.take() {
                    buffer.extend(capture.samples.try_iter().flatten());
                    drop(capture);
                    state(DictationState::Transcribing);
                    let text = voice.transcribe_locked(&buffer)?;
                    buffer.clear();
                    if !text.is_empty() {
                        events.send(Event::Final { text }).ok();
                    }
                    state(DictationState::Idle);
                }
            }
            Ok(Command::Cancel) => {
                if audio.take().is_some() {
                    buffer.clear();
                    state(DictationState::Idle);
                }
            }
            Ok(Command::Quit) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if let Some(capture) = &audio {
            if let Ok(chunk) = capture.samples.recv_timeout(Duration::from_millis(50)) {
                buffer.extend_from_slice(&chunk);
            }
        }
    }
}
