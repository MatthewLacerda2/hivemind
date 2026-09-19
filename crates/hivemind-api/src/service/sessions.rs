//! Which Claude Code sessions are open on **this** machine (SPEC §9.3).
//!
//! A session is presence, never an address. The identity is the machine
//! (ADR 0003), mail is delivered to the node, and any session may read it.
//! What this adds is information: how many there are, and what each is
//! working on, so a Claude deciding who to write to can tell "that machine is
//! on" from "there is somebody in the repo I am asking about".
//!
//! # Why the hooks and not the MCP server
//!
//! The MCP server here is streamable HTTP, and `server.rs` says so in as many
//! words: a session lasts as long as the daemon does, and a Claude that
//! reconnects starts another. There is no reliable "it went away". The hooks
//! run at every turn boundary and are already installed, so expiry by time is
//! the truth and `SessionEnd` is a courtesy that closes one sooner.
//!
//! Nothing here is persisted. A daemon that restarts knows about no sessions
//! until each one's next turn, which is at most one prompt away and is more
//! honest than a file claiming something about a process that may be gone.

use std::collections::HashMap;

use super::{DateTime, MailService, Utc};
use crate::peer::SessionNote;

/// How long a session lives without a sign of life (SPEC §9.3).
///
/// Long enough that somebody reading a long diff between prompts is still
/// working, short enough that a terminal closed without `SessionEnd` stops
/// being advertised within the hour. Not configurable until somebody has a
/// reason: a knob with no incident behind it is a second thing to explain.
pub const SESSION_TTL: std::time::Duration = std::time::Duration::from_mins(30);

/// One open session.
#[derive(Debug, Clone)]
pub struct Session {
    /// What it is working on — the basename of its working directory.
    pub label: String,
    /// When it last showed a sign of life.
    pub last_seen: DateTime<Utc>,
}

impl MailService {
    /// Register a session, or renew one already known (SPEC §9.3).
    ///
    /// `SessionStart` registers and `UserPromptSubmit` renews, and both arrive
    /// here: renewing something unknown registers it, because a daemon that
    /// restarted mid-conversation missed the start and the next prompt is the
    /// first it hears of a session that is plainly alive.
    pub fn register_session(&self, id: &str, label: &str) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        // Swept on write rather than capped. The register is fed only over the
        // loopback API, and every entry expires, so it is already bounded by
        // "sessions that had a turn in the last half hour" — which on a real
        // machine is a handful. A cap would add a way to drop a live session
        // in exchange for nothing.
        let now = Utc::now();
        sessions.retain(|_, session| open_at(now, session.last_seen, SESSION_TTL));
        sessions.insert(
            id.to_owned(),
            Session {
                label: label.to_owned(),
                last_seen: now,
            },
        );
    }

    /// Close a session. Returns whether there was one.
    ///
    /// `SessionEnd` is a courtesy — expiry is what makes the list true — so a
    /// call for a session nobody registered is not an error.
    pub fn end_session(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .is_ok_and(|mut sessions| sessions.remove(id).is_some())
    }

    /// Every session still open, oldest sign of life last.
    #[must_use]
    pub fn open_sessions(&self) -> Vec<Session> {
        let Ok(sessions) = self.sessions.lock() else {
            return Vec::new();
        };
        let now = Utc::now();
        let mut open: Vec<Session> = sessions
            .values()
            .filter(|session| open_at(now, session.last_seen, SESSION_TTL))
            .cloned()
            .collect();
        // Most recently active first, and by label after that so two sessions
        // that renewed in the same millisecond do not swap places between
        // calls — a peer list that reorders itself is read as a change.
        open.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.label.cmp(&b.label))
        });
        open
    }

    /// The sessions as a hello carries them (SPEC §5.5).
    ///
    /// Labels only. The id is what the hooks on this machine use to renew and
    /// close a registration; another node has nothing it could do with one.
    #[must_use]
    pub(crate) fn session_notes(&self) -> Vec<SessionNote> {
        self.open_sessions()
            .into_iter()
            .map(|session| SessionNote {
                label: session.label,
            })
            .collect()
    }
}

/// Is a session with this last sign of life still open?
///
/// Split from the clock so both sides of the boundary can be tested without
/// waiting half an hour, which is the shape `still_here` and `doctor`'s
/// optional-tools rule took for the same reason.
fn open_at(now: DateTime<Utc>, last_seen: DateTime<Utc>, ttl: std::time::Duration) -> bool {
    let Ok(age) = now.signed_duration_since(last_seen).to_std() else {
        // Last seen in the future: a clock that moved, not a session that
        // ended. Believing it is over would close every session on a machine
        // whose time was corrected backwards.
        return true;
    };
    age < ttl
}

/// The register, in memory and nowhere else.
pub(super) type Sessions = HashMap<String, Session>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::service;
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("in range")
    }

    #[test]
    fn a_session_is_open_until_its_time_runs_out_and_not_after() {
        let ttl = Duration::from_mins(30);
        assert!(open_at(at(1_000), at(1_000), ttl), "just registered");
        assert!(open_at(at(1_000 + 29 * 60), at(1_000), ttl));
        assert!(
            !open_at(at(1_000 + 30 * 60), at(1_000), ttl),
            "exactly the limit is already over"
        );
    }

    #[test]
    fn registering_a_session_puts_it_on_the_list_with_its_label() {
        let (_dir, service) = service();
        assert!(service.open_sessions().is_empty(), "none to begin with");

        service.register_session("01JXT-a", "hivemind");

        let open = service.open_sessions();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].label, "hivemind");
    }

    #[test]
    fn two_sessions_are_two_sessions_even_in_the_same_repo() {
        // Two terminals in one checkout is the ordinary case, and the point
        // of the feature is seeing how many Claudes are about.
        let (_dir, service) = service();
        service.register_session("01JXT-a", "hivemind");
        service.register_session("01JXT-b", "hivemind");

        assert_eq!(service.open_sessions().len(), 2);
    }

    #[test]
    fn renewing_a_session_does_not_make_a_second_one() {
        // `UserPromptSubmit` runs on every turn. Without this a long
        // conversation would look like forty Claudes in one repo.
        let (_dir, service) = service();
        service.register_session("01JXT-a", "hivemind");
        service.register_session("01JXT-a", "hivemind");
        service.register_session("01JXT-a", "hivemind");

        assert_eq!(service.open_sessions().len(), 1);
    }

    #[test]
    fn a_session_that_moved_is_relabelled_rather_than_duplicated() {
        // The same id with a different `cwd`. Rare, but a stale label is a
        // wrong answer to the one question this feature exists to answer.
        let (_dir, service) = service();
        service.register_session("01JXT-a", "hivemind");
        service.register_session("01JXT-a", "scorsese");

        let open = service.open_sessions();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].label, "scorsese");
    }

    #[test]
    fn ending_a_session_takes_it_off_the_list() {
        let (_dir, service) = service();
        service.register_session("01JXT-a", "hivemind");

        assert!(service.end_session("01JXT-a"));
        assert!(service.open_sessions().is_empty());
    }

    #[test]
    fn ending_a_session_nobody_registered_is_not_an_error() {
        // `SessionEnd` is a courtesy; expiry is what makes the list true. A
        // daemon that restarted mid-conversation never saw the start.
        let (_dir, service) = service();
        assert!(!service.end_session("01JXT-never-seen"));
    }

    #[test]
    fn a_hello_carries_the_labels_and_never_the_ids() {
        // The id is what the hooks on this machine use to renew and close a
        // registration. Another node has nothing it could do with one.
        let (_dir, service) = service();
        service.register_session("01JXT-secret", "hivemind");

        let notes = service.session_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].label, "hivemind");

        let encoded = serde_json::to_string(&notes).expect("serialise");
        assert!(
            !encoded.contains("01JXT-secret"),
            "the id must not travel: {encoded}"
        );
    }

    #[test]
    fn a_clock_correction_does_not_close_every_session_on_the_machine() {
        // `last_seen` in the future means the machine's time moved, not that
        // somebody worked tomorrow. Treating it as expired would clear the
        // register on any host that just got an NTP correction.
        assert!(open_at(at(1_000), at(9_999), Duration::from_mins(30)));
    }
}
