//! Tracks when each file was last read, so edit tools can enforce
//! "you must read a file before editing it" and refuse edits when the file
//! changed on disk since that read (external edit → stale view).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Default)]
pub struct FileTracker {
    /// absolute path → mtime at the moment the agent last read it.
    reads: Mutex<HashMap<String, SystemTime>>,
}

impl FileTracker {
    pub fn new() -> Self {
        FileTracker::default()
    }

    pub fn record_read(&self, path: &str) {
        if let Some(m) = mtime(path) {
            self.reads.lock().unwrap().insert(path.to_string(), m);
        }
    }

    /// After a successful write/edit, refresh the tracked mtime.
    pub fn record_write(&self, path: &str) {
        if let Some(m) = mtime(path) {
            self.reads.lock().unwrap().insert(path.to_string(), m);
        }
    }

    /// Returns None if editable, or Some(error) explaining why not.
    pub fn check_editable(&self, path: &str) -> Option<String> {
        let last = { self.reads.lock().unwrap().get(path).copied() };
        match last {
            None => Some(format!(
                "file has not been read yet; read \"{}\" before editing it",
                path
            )),
            Some(last) => {
                if let Some(current) = mtime(path) {
                    if current > last {
                        return Some(format!(
                            "file \"{}\" was modified since it was last read; read it again before editing",
                            path
                        ));
                    }
                }
                None
            }
        }
    }
}

fn mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(Path::new(path))
        .ok()
        .and_then(|m| m.modified().ok())
}
