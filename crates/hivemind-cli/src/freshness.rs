//! Is the daemon running the binary that is installed? (#36)
//!
//! Reinstalling replaces the file; the running daemon goes on serving the code
//! it loaded, and `hivemind status` says `v0.1.0` before and after, because the
//! version does not move between two builds of one release. So the person who
//! has just fixed something checks whether it worked against the code they were
//! trying to replace — at the moment the answer matters most and is doubted
//! least — and reports it with confidence. It is the false green, and it is
//! guaranteed to recur.
//!
//! The daemon says when the binary it started from was written, on every local
//! API response (`hivemind_api::freshness::BINARY_MODIFIED`). This compares
//! that with the binary now on disk, which is the one running this process: the
//! roadmap's "a daemon older than the binary invoking it".
//!
//! The judgement is [`is_stale`] and nothing else, split from the two lookups
//! that feed it, because on any machine where this gets written nothing has
//! been reinstalled — so a test driven through the lookups could only ever
//! reach the uninteresting half.

use chrono::{DateTime, Utc};

use crate::colour::Paint as _;

/// The one line a command prints when it is talking to a stale daemon.
///
/// One line, because it interrupts something somebody else asked for. On
/// stderr, because `--json` output is read by scripts and by other Claudes
/// (#55) and must stay parseable.
pub(crate) const WARNING: &str = "warning: the daemon is running an older binary than this one; \
     `hivemind service restart` picks it up";

/// What `doctor` says to do about it, where there is room to cover the person
/// who never installed a service and runs it in a terminal.
pub(crate) const RESTART: &str =
    "restart it: `hivemind service restart`, or stop and start `hivemind daemon`";

/// Is the daemon running an older binary than the one asking?
///
/// Unknown is never stale. A missing timestamp is a question this cannot
/// answer, and answering it anyway would put a warning on every run — which is
/// how a warning stops being read.
pub(crate) fn is_stale(daemon: Option<DateTime<Utc>>, on_disk: Option<DateTime<Utc>>) -> bool {
    matches!((daemon, on_disk), (Some(daemon), Some(on_disk)) if on_disk > daemon)
}

/// The timestamp the daemon stamped on its response, if it sent one this
/// version understands.
pub(crate) fn from_header(value: Option<&str>) -> Option<DateTime<Utc>> {
    Some(
        DateTime::parse_from_rfc3339(value?)
            .ok()?
            .with_timezone(&Utc),
    )
}

/// Say it, once, if the daemon that just answered is behind this binary.
///
/// Once per process: a command can make several requests, and the person needs
/// telling rather than nagging.
pub(crate) fn note(header: Option<&str>) {
    static SAID: std::sync::Once = std::sync::Once::new();

    if is_stale(from_header(header), hivemind_core::binary::ours()) {
        SAID.call_once(|| eprintln!("{}", WARNING.yellow()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instant, `seconds` after a fixed one. The absolute value means
    /// nothing; the gap between two of them is the whole of what is tested.
    fn at(seconds: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0)
    }

    #[test]
    fn a_binary_written_after_the_daemon_started_is_stale() {
        // The whole of #36: `cargo install` replaced the file, the daemon
        // kept serving what it had loaded.
        assert!(is_stale(at(0), at(60)));
    }

    #[test]
    fn the_binary_the_daemon_started_from_is_not_stale() {
        // The ordinary case, and by far the common one: nobody reinstalled,
        // so both sides read the same file and the same instant.
        assert!(!is_stale(at(0), at(0)));
    }

    #[test]
    fn a_daemon_newer_than_the_binary_asking_is_not_stale() {
        // An old CLI left on PATH beside a daemon that was restarted since.
        // Worth not shouting about: the daemon is the newer of the two, which
        // is the direction this check exists to catch, backwards.
        assert!(!is_stale(at(60), at(0)));
    }

    #[test]
    fn a_timestamp_nobody_could_read_is_never_stale() {
        // Both directions of unknown. A warning on every run is a warning
        // nobody reads, and this would fire on every run of an old daemon
        // that does not send the header at all.
        assert!(!is_stale(None, at(60)));
        assert!(!is_stale(at(0), None));
        assert!(!is_stale(None, None));
    }

    #[test]
    fn the_header_is_read_as_the_instant_it_names() {
        let parsed = from_header(Some("2023-11-14T22:13:20Z")).expect("RFC 3339");
        assert_eq!(parsed.timestamp(), 1_700_000_000);
    }

    #[test]
    fn a_header_that_is_absent_or_nonsense_reads_as_unknown() {
        // A daemon from before this change sends nothing, and it must not be
        // reported as anything: this runs on every response.
        assert_eq!(from_header(None), None);
        assert_eq!(from_header(Some("")), None);
        assert_eq!(from_header(Some("last Tuesday")), None);
    }

    #[test]
    fn the_warning_says_what_happened_and_what_to_do() {
        // A check that can only say "failed" is not worth running, and this
        // one gets seen in the middle of a command about something else.
        assert!(WARNING.contains("older binary"), "{WARNING}");
        assert!(WARNING.contains("restart"), "{WARNING}");
        assert!(!WARNING.contains('\n'), "one line: {WARNING}");
    }
}
