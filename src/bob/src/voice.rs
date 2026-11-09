//! Local voice dictation: model management for `bob voice` and a push-to-talk
//! recorder for the composer. Compiled in on macOS only.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Model {
    pub name: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

pub const BASE: Model = Model {
    name: "base.en",
    file: "ggml-base.en.bin",
    bytes: 147_964_211,
    sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
};

pub const TURBO: Model = Model {
    name: "large-v3-turbo",
    file: "ggml-large-v3-turbo.bin",
    bytes: 1_624_555_275,
    sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
};

const MODELS: [&Model; 2] = [&TURBO, &BASE];

pub fn models_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join(".bob")
        .join("models"))
}

fn installed_in(dir: &Path) -> Vec<(&'static Model, PathBuf)> {
    MODELS
        .into_iter()
        .filter_map(|model| {
            let path = dir.join(model.file);
            path.is_file().then_some((model, path))
        })
        .collect()
}

/// Applies `[voice] model` from the config: a named model must be installed;
/// unset or "auto" uses the most accurate installed model.
fn choose(
    setting: Option<&str>,
    installed: Vec<(&'static Model, PathBuf)>,
) -> Result<(&'static Model, PathBuf)> {
    match setting
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "auto")
    {
        Some(name) => {
            let Some(model) = MODELS.into_iter().find(|model| model.name == name) else {
                bail!(
                    "unknown voice model {name:?} in [voice] model; use \"base.en\", \"large-v3-turbo\" or \"auto\""
                );
            };
            installed
                .into_iter()
                .find(|(installed, _)| installed.name == model.name)
                .with_context(|| {
                    format!(
                        "voice model {name} is selected in [voice] model but not installed; run `bob voice install{}`",
                        if model.name == TURBO.name { " --turbo" } else { "" }
                    )
                })
        }
        None => installed
            .into_iter()
            .next()
            .context("no voice model installed; run `bob voice install` in a shell"),
    }
}

/// The model dictation will use, from the effective config for `workspace`.
pub fn selected(workspace: &Path) -> Result<(&'static Model, PathBuf)> {
    let config = bob_core::core::config::load_config(workspace)?;
    choose(config.voice.model.as_deref(), installed_in(&models_dir()?))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn install(turbo: bool, from: Option<PathBuf>) -> Result<()> {
    let model = if turbo { &TURBO } else { &BASE };
    let dir = models_dir()?;
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(model.file);
    if target.is_file() && sha256_file(&target)? == model.sha256 {
        println!(
            "{} is already installed at {}",
            model.name,
            target.display()
        );
        return Ok(());
    }
    let partial = dir.join(format!("{}.partial", model.file));
    if let Some(source) = from {
        let source = if source.is_dir() {
            source.join(model.file)
        } else {
            source
        };
        println!("Copying {} …", source.display());
        std::fs::copy(&source, &partial).with_context(|| format!("copy {}", source.display()))?;
    } else {
        let url = format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            model.file
        );
        println!(
            "Downloading {} ({} MB) from {url}",
            model.name,
            model.bytes / 1_000_000
        );
        let mut response = reqwest::get(&url).await?.error_for_status()?;
        let mut out = std::fs::File::create(&partial)?;
        let mut done: u64 = 0;
        let mut shown = u64::MAX;
        while let Some(chunk) = response.chunk().await? {
            out.write_all(&chunk)?;
            done += chunk.len() as u64;
            let percent = done * 100 / model.bytes.max(1);
            if percent != shown {
                shown = percent;
                print!(
                    "\r  {percent:>3}%  {} / {} MB",
                    done / 1_000_000,
                    model.bytes / 1_000_000
                );
                std::io::stdout().flush().ok();
            }
        }
        println!();
        out.sync_all()?;
    }
    print!("Verifying checksum … ");
    std::io::stdout().flush().ok();
    let digest = sha256_file(&partial)?;
    if digest != model.sha256 {
        std::fs::remove_file(&partial).ok();
        bail!(
            "checksum mismatch for {} (expected {}, got {digest}); nothing was installed",
            model.name,
            model.sha256
        );
    }
    std::fs::rename(&partial, &target)?;
    println!("ok");
    println!(
        "Installed {} at {}. Press Ctrl+R in bob's input box to dictate.",
        model.name,
        target.display()
    );
    Ok(())
}

pub fn status() -> Result<()> {
    let dir = models_dir()?;
    let mut any = false;
    for model in MODELS {
        let path = dir.join(model.file);
        if path.is_file() {
            any = true;
            println!("installed  {:<15} {}", model.name, path.display());
        }
    }
    if !any {
        println!("No voice model installed. Run `bob voice install`.");
        return Ok(());
    }
    match selected(&std::env::current_dir()?) {
        Ok((model, _)) => println!("active     {}", model.name),
        Err(error) => println!("active     none: {error:#}"),
    }
    match bob_voice::audio::AudioCapture::start() {
        Ok(_) => println!("microphone ok"),
        Err(error) => println!("microphone unavailable: {error}"),
    }
    Ok(())
}

pub fn remove() -> Result<()> {
    let dir = models_dir()?;
    let mut removed = 0;
    for model in MODELS {
        for name in [model.file.to_owned(), format!("{}.partial", model.file)] {
            let path = dir.join(name);
            if path.is_file() {
                std::fs::remove_file(&path)?;
                println!("removed {}", path.display());
                removed += 1;
            }
        }
    }
    if removed == 0 {
        println!("No voice models to remove.");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Loading,
    Listening,
    Transcribing,
    Text(String),
    Idle,
    Error(String),
}

enum Control {
    Start,
    Stop,
    Cancel,
}

/// Push-to-talk recorder. The model loads once in the background on first use
/// and stays loaded; the microphone is open only while recording.
pub struct Recorder {
    commands: std::sync::mpsc::Sender<Control>,
    updates: std::sync::mpsc::Receiver<Update>,
}

impl Recorder {
    pub fn spawn(workspace: &Path) -> Result<Self> {
        let (_, path) = selected(workspace)?;
        let (commands, command_rx) = std::sync::mpsc::channel::<Control>();
        let (update_tx, updates) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let send = |update| update_tx.send(update).is_ok();
            if !send(Update::Loading) {
                return;
            }
            let voice = match bob_voice::Voice::builder()
                .model(&path)
                .language("en")
                .build()
            {
                Ok(voice) => voice,
                Err(error) => {
                    send(Update::Error(format!(
                        "voice model failed to load: {error:#}"
                    )));
                    return;
                }
            };
            let session = match voice.into_session() {
                Ok(session) => session,
                Err(error) => {
                    send(Update::Error(format!("{error:#}")));
                    return;
                }
            };
            send(Update::Idle);
            loop {
                while let Ok(event) = session.receiver().try_recv() {
                    let update = match event {
                        bob_voice::Event::State { state } => match state {
                            bob_voice::DictationState::Listening => Update::Listening,
                            bob_voice::DictationState::Transcribing => Update::Transcribing,
                            _ => Update::Idle,
                        },
                        bob_voice::Event::Final { text } => Update::Text(text),
                        bob_voice::Event::Error { message } => {
                            Update::Error(format!("voice: {message}"))
                        }
                        bob_voice::Event::Partial { .. } => continue,
                    };
                    if !send(update) {
                        return;
                    }
                }
                match command_rx.recv_timeout(std::time::Duration::from_millis(30)) {
                    Ok(Control::Start) => session.start(),
                    Ok(Control::Stop) => session.stop(),
                    Ok(Control::Cancel) => session.cancel(),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Ok(Self { commands, updates })
    }

    pub fn start(&self) {
        let _ = self.commands.send(Control::Start);
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Control::Stop);
    }

    pub fn cancel(&self) {
        let _ = self.commands.send(Control::Cancel);
    }

    pub fn poll(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Vec<(&'static Model, PathBuf)> {
        vec![(&TURBO, "turbo".into()), (&BASE, "base".into())]
    }

    #[test]
    fn auto_prefers_the_most_accurate_installed_model() {
        for setting in [None, Some("auto"), Some(" ")] {
            assert_eq!(choose(setting, both()).unwrap().0.name, TURBO.name);
        }
        let base_only = vec![(&BASE, PathBuf::from("base"))];
        assert_eq!(choose(None, base_only).unwrap().0.name, BASE.name);
        assert!(choose(None, Vec::new()).is_err());
    }

    #[test]
    fn a_named_model_must_exist_and_be_installed() {
        assert_eq!(choose(Some("base.en"), both()).unwrap().0.name, BASE.name);
        let missing = choose(Some("large-v3-turbo"), vec![(&BASE, "base".into())])
            .unwrap_err()
            .to_string();
        assert!(missing.contains("bob voice install --turbo"), "{missing}");
        let unknown = choose(Some("tiny"), both()).unwrap_err().to_string();
        assert!(unknown.contains("unknown voice model"), "{unknown}");
    }
}
