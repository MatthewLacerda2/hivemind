//! Which binary this daemon is actually running (#36).
//!
//! Reinstalling — `cargo install --path …`, `brew upgrade`, a `git pull` and a
//! rebuild — replaces the file the daemon was started from. The running process
//! is untouched: it goes on serving the code it loaded, and goes on saying
//! `v0.1.0` about itself, because the version does not move between two builds
//! of one release. So the person who has just fixed something checks whether it
//! worked against the code they were trying to replace, and reports the result
//! with confidence.
//!
//! The modification time of the file is the one thing that does differ, so the
//! daemon records it at startup — while the file on disk is still the one it
//! loaded — and reports it two ways: on `/api/v1/me`, for anything that asks,
//! and as [`BINARY_MODIFIED`] on every local API response, so that a CLI
//! command can notice without spending a round-trip on a question it did not
//! come to ask.
//!
//! A process-wide `OnceLock` rather than a field on the mail service, because
//! this is a fact about the process and not about the mail. What matters about
//! it is that it is read once, early, and never again — which a field cannot
//! promise any better and a signature change would spread over five call sites
//! that have no interest in it.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};

/// The header every local API response carries: an RFC 3339 timestamp saying
/// when the running daemon's binary was written.
///
/// Named once, here, because the CLI reads what this crate writes and a header
/// spelled twice is a header spelled two ways eventually.
pub const BINARY_MODIFIED: &str = "x-hivemind-binary-modified";

static STARTED_FROM: OnceLock<Option<DateTime<Utc>>> = OnceLock::new();

/// Record the binary this process was started from.
///
/// Called once, as early in `hivemind daemon` as there is anything to call,
/// so the answer is the file that was loaded rather than whatever is at that
/// path by the time somebody asks. Calling it twice changes nothing, which is
/// the point of doing it this way round.
pub fn remember() {
    let _ = STARTED_FROM.set(hivemind_core::binary::ours());
}

/// When the binary this daemon started from was written.
///
/// `None` when nothing remembered it — an in-process router in a test, rather
/// than a daemon — or when the file could not be read at all. Both are
/// "unknown", and unknown is never reported as stale.
#[must_use]
pub fn started_from() -> Option<DateTime<Utc>> {
    STARTED_FROM.get().copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_that_has_remembered_its_binary_can_report_it() {
        // The one test in this module that touches the lock, because the lock
        // is process-wide: a second one asserting the unset case would pass or
        // fail on which of the two ran first.
        remember();

        assert_eq!(started_from(), hivemind_core::binary::ours());
        assert!(
            started_from().is_some(),
            "the running test binary is a file on disk on both platforms"
        );
    }
}
