//! Content snapshots and visible byte coverage for guarded edits.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone)]
struct ReadSnapshot {
    hash: [u8; 32],
    length: usize,
    visible: Vec<Range<usize>>,
}

#[derive(Default)]
pub struct FileTracker {
    reads: Mutex<HashMap<String, ReadSnapshot>>,
}

fn key(path: &str) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| Path::new(path).to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl FileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_visible(&self, path: &str, content: &str, visible: Vec<Range<usize>>) {
        let hash: [u8; 32] = Sha256::digest(content.as_bytes()).into();
        let mut reads = self.reads.lock().unwrap();
        let snapshot = reads.entry(key(path)).or_insert_with(|| ReadSnapshot {
            hash,
            length: content.len(),
            visible: Vec::new(),
        });
        if snapshot.hash != hash {
            *snapshot = ReadSnapshot {
                hash,
                length: content.len(),
                visible: Vec::new(),
            };
        }
        snapshot.visible.extend(
            visible
                .into_iter()
                .filter(|range| range.start <= range.end && range.end <= content.len()),
        );
        snapshot.visible.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for range in snapshot.visible.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        snapshot.visible = merged;
    }

    pub fn record_read(&self, path: &str) {
        if let Ok(content) = crate::tools::filesystem::read_regular(Path::new(&key(path))) {
            self.record_visible(path, &content, std::iter::once(0..content.len()).collect());
        }
    }

    pub fn record_content(&self, path: &str, content: &str, fully_known: bool) {
        let visible = if fully_known {
            std::iter::once(0..content.len()).collect()
        } else {
            Vec::new()
        };
        self.reads.lock().unwrap().insert(
            key(path),
            ReadSnapshot {
                hash: Sha256::digest(content.as_bytes()).into(),
                length: content.len(),
                visible,
            },
        );
    }

    pub fn record_write(&self, path: &str) {
        self.reads.lock().unwrap().remove(&key(path));
    }

    pub fn fully_read(&self, path: &str, content: &str) -> bool {
        self.check_snapshot(path, content, true).is_none()
    }

    pub fn check_snapshot(
        &self,
        path: &str,
        content: &str,
        require_complete: bool,
    ) -> Option<String> {
        let hash: [u8; 32] = Sha256::digest(content.as_bytes()).into();
        let reads = self.reads.lock().unwrap();
        let Some(snapshot) = reads.get(&key(path)) else {
            return Some(format!(
                "file has not been read yet; read \"{path}\" before editing it"
            ));
        };
        if snapshot.hash != hash {
            return Some(format!(
                "file \"{path}\" was modified since it was last read; read it again before editing"
            ));
        }
        if require_complete
            && !snapshot
                .visible
                .iter()
                .any(|range| range.start == 0 && range.end == snapshot.length)
        {
            return Some(format!("file \"{path}\" has not been fully shown; read the remaining content or use edit_file instead of overwriting"));
        }
        None
    }

    pub fn check_editable(&self, path: &str) -> Option<String> {
        match crate::tools::filesystem::read_regular(Path::new(&key(path))) {
            Ok(content) => self.check_snapshot(path, &content, false),
            Err(error) => Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture as Fixture;
    use std::fs::File;

    #[test]
    fn coverage_merges_overlapping_adjacent_and_duplicate_ranges_but_not_gaps() {
        let tracker = FileTracker::new();
        let content = "0123456789";
        tracker.record_visible("tracked", content, vec![7..10, 2..4, 0..2, 2..4, 1..3]);
        assert_eq!(
            tracker.reads.lock().unwrap()[&key("tracked")].visible,
            vec![0..4, 7..10]
        );
        assert_eq!(tracker.check_snapshot("tracked", content, false), None);
        assert!(tracker
            .check_snapshot("tracked", content, true)
            .unwrap()
            .contains("not been fully shown"));

        tracker.record_visible("tracked", content, std::iter::once(4..7).collect());
        assert_eq!(
            tracker.reads.lock().unwrap()[&key("tracked")].visible,
            std::iter::once(0..10).collect::<Vec<_>>()
        );
        assert!(tracker.fully_read("tracked", content));
    }

    #[test]
    fn invalid_ranges_cannot_fill_a_valid_coverage_gap() {
        let tracker = FileTracker::new();
        let reversed = Range { start: 4, end: 2 };
        tracker.record_visible("tracked", "abcdef", vec![0..2, reversed, 2..7, 7..7, 6..6]);
        assert_eq!(
            tracker.reads.lock().unwrap()[&key("tracked")].visible,
            vec![0..2, 6..6]
        );
        assert!(!tracker.fully_read("tracked", "abcdef"));
        tracker.record_visible("tracked", "abcdef", std::iter::once(2..6).collect());
        assert!(tracker.fully_read("tracked", "abcdef"));
    }

    #[test]
    fn unicode_coverage_uses_byte_ranges_and_preserves_an_unseen_scalar() {
        let tracker = FileTracker::new();
        let content = "é🙂終\n";
        tracker.record_visible("tracked", content, vec![0..2, 6..10]);
        assert!(!tracker.fully_read("tracked", content));
        tracker.record_visible("tracked", content, std::iter::once(2..6).collect());
        assert!(tracker.fully_read("tracked", content));
        assert_eq!(
            tracker.reads.lock().unwrap()[&key("tracked")].visible,
            std::iter::once(0..10).collect::<Vec<_>>()
        );
    }

    #[test]
    fn changed_snapshot_discards_old_coverage_even_when_length_is_unchanged() {
        let tracker = FileTracker::new();
        tracker.record_visible("tracked", "abcdef", std::iter::once(0..3).collect());
        tracker.record_visible(
            "tracked",
            "ABCdef",
            std::iter::once(3..6).collect::<Vec<_>>(),
        );
        assert_eq!(
            tracker.reads.lock().unwrap()[&key("tracked")].visible,
            std::iter::once(3..6).collect::<Vec<_>>()
        );
        assert!(!tracker.fully_read("tracked", "ABCdef"));
        assert!(tracker
            .check_snapshot("tracked", "abcdef", false)
            .unwrap()
            .contains("modified since"));
        tracker.record_visible("tracked", "ABCdef", std::iter::once(0..3).collect());
        assert!(tracker.fully_read("tracked", "ABCdef"));
    }

    #[test]
    fn stale_checks_do_not_replace_the_last_observed_snapshot() {
        let tracker = FileTracker::new();
        assert!(tracker
            .check_snapshot("tracked", "old", false)
            .unwrap()
            .contains("not been read"));
        tracker.record_content("tracked", "old", true);
        for require_complete in [false, true] {
            assert!(tracker
                .check_snapshot("tracked", "new", require_complete)
                .unwrap()
                .contains("modified since"));
        }
        assert!(tracker.fully_read("tracked", "old"));
        assert!(tracker
            .check_snapshot("other", "old", false)
            .unwrap()
            .contains("not been read"));
    }

    #[test]
    fn writes_reset_coverage_without_promoting_partially_known_contents() {
        let tracker = FileTracker::new();
        tracker.record_content("tracked", "before", true);
        tracker.record_content("tracked", "after", false);
        assert_eq!(tracker.check_snapshot("tracked", "after", false), None);
        assert!(!tracker.fully_read("tracked", "after"));
        tracker.record_visible("tracked", "after", std::iter::once(0..5).collect());
        assert!(tracker.fully_read("tracked", "after"));
        tracker.record_write("tracked");
        assert!(tracker
            .check_snapshot("tracked", "after", false)
            .unwrap()
            .contains("not been read"));
    }

    #[test]
    fn empty_files_still_require_an_observed_empty_range_for_overwrite() {
        let tracker = FileTracker::new();
        tracker.record_content("tracked", "", false);
        assert!(!tracker.fully_read("tracked", ""));
        tracker.record_visible("tracked", "", std::iter::once(0..0).collect());
        assert!(tracker.fully_read("tracked", ""));
    }

    #[test]
    fn tracker_detects_content_change_even_with_backdated_mtime() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "before");
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        fixture.ctx.files.record_read(&path.to_string_lossy());
        std::fs::write(&path, "after!").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before))
            .unwrap();
        assert!(fixture
            .ctx
            .files
            .check_editable(&path.to_string_lossy())
            .is_some());
    }
}
