//! When the file a process was started from was last written.
//!
//! A running daemon serves the code it loaded at startup. Reinstalling
//! replaces the file on disk and changes nothing in the process, and
//! `CARGO_PKG_VERSION` does not move between two builds of one release — so
//! `v0.1.0` before and `v0.1.0` after, and a version comparison cannot see it
//! either. The modification time of the file is the one thing that does
//! differ, which is why it is what gets compared (#36).
//!
//! Filesystem only, so this belongs here (SPEC §3.1).
//! [`std::env::current_exe`] is the whole of the platform-specific part, and it
//! is why this is not `/proc/self/exe`: macOS has no `/proc`, so that would
//! have worked on CI and on none of the machines hivemind is written on.

use std::path::Path;

use chrono::{DateTime, Utc};

/// When the file at `path` was last modified, or `None` if that cannot be read.
///
/// A lookup, kept apart from the judgement that uses it so that both answers
/// are reachable on a machine where nothing has been reinstalled.
#[must_use]
pub fn modified_at(path: &Path) -> Option<DateTime<Utc>> {
    Some(std::fs::metadata(path).ok()?.modified().ok()?.into())
}

/// When the file this process was started from was last modified.
///
/// Read at the moment it is called and never cached, because the two callers
/// want different things from it: a daemon asks once at startup, while the file
/// on disk is still the one it loaded, and keeps the answer; a short-lived CLI
/// process asks in order to learn what is on disk *now*.
#[must_use]
pub fn ours() -> Option<DateTime<Utc>> {
    modified_at(&std::env::current_exe().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_reports_when_it_was_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hivemind");
        let before = Utc::now();
        std::fs::write(&path, b"not really a binary").expect("write");
        let after = Utc::now();

        let modified = modified_at(&path).expect("a file that exists has a modification time");
        // A second of slack each way: not every filesystem keeps more
        // precision than that, and HFS+ keeps exactly one second.
        let slack = chrono::Duration::seconds(1);
        assert!(
            modified >= before - slack && modified <= after + slack,
            "{modified} is not when the file was written ({before} to {after})"
        );
    }

    #[test]
    fn a_path_with_nothing_at_it_has_no_modification_time() {
        // The branch that decides whether a missing binary reads as "stale".
        // It must be an absent answer rather than a panic or an epoch.
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(modified_at(&dir.path().join("never-written")), None);
    }

    #[test]
    fn this_process_knows_the_file_it_was_started_from() {
        // On both platforms: the test binary exists on disk wherever this runs.
        let ours = ours().expect("the running test binary is a file on disk");
        let exe = std::env::current_exe().expect("current_exe");
        assert_eq!(Some(ours), modified_at(&exe));
    }
}
