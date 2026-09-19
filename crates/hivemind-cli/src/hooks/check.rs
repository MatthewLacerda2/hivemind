//! What `hivemind hook check` does at a Claude turn boundary (SPEC §9.3).
//!
//! The other half of `hooks` — `mod.rs` puts the hooks into
//! `~/.claude/settings.json`; this is what they run.
//!
//! Two jobs in one command, because Claude Code runs one command per event
//! and both want the same turn boundary: print a line if there is unread
//! mail, and tell the daemon this session is alive and what it is working on.
//!
//! **Nothing here may fail, and nothing here may be slow.** A hook that
//! errors interrupts somebody's work to report something they did not ask
//! about, and the budget for the whole thing is a hundred milliseconds. Every
//! failure below is therefore silence.

use std::path::Path;

use hivemind_core::index::Index;

use crate::commands::{short_node, unread_phrase};
use crate::paths;

/// What Claude Code hands a hook on stdin (SPEC §9.3).
///
/// Only the three fields hivemind uses. Claude Code sends more, and a hook
/// that refused to parse an unfamiliar document would break on the next
/// version of something it does not own.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HookEvent {
    /// The session this turn belongs to.
    pub(crate) session_id: String,
    /// What it is working on: the basename of the working directory.
    pub(crate) label: String,
    /// Whether this is the end of the session rather than a turn in it.
    pub(crate) ending: bool,
}

/// What Claude Code wrote to this process's stdin, if anything.
///
/// A terminal is not read from at all: somebody running `hivemind hook check`
/// by hand to see what it prints would otherwise be left staring at a hung
/// process waiting for them to type a JSON document and press ctrl-D.
fn read_hook_payload() -> String {
    use std::io::{IsTerminal as _, Read as _};

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return String::new();
    }

    let mut payload = String::new();
    // A hook payload is a few hundred bytes. The cap is not about this one;
    // it is that a hook must finish, and an unbounded read from a pipe
    // somebody else owns is not a thing that has to.
    let mut limited = std::io::Read::take(&mut stdin, 64 * 1024);
    let _ = limited.read_to_string(&mut payload);
    payload
}

/// Read what the hook was told, if it was told anything usable.
///
/// Pure, because the interesting cases are all documents — an event with no
/// `cwd`, a `cwd` that is the filesystem root, a payload from a version of
/// Claude Code that sends a field we have never seen — and none of them are
/// worth a process to produce.
pub(crate) fn hook_event(payload: &str) -> Option<HookEvent> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let session_id = value.get("session_id")?.as_str()?.trim().to_owned();
    if session_id.is_empty() {
        return None;
    }

    let cwd = value.get("cwd").and_then(serde_json::Value::as_str);
    Some(HookEvent {
        session_id,
        label: label_for(cwd),
        ending: value
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("SessionEnd")),
    })
}

/// What to call a session working in `cwd`.
///
/// The basename, because that is what a person calls the thing they are in
/// and the whole path is both longer than a peer list wants and more than
/// somebody else's machine needs to know. A root directory has no basename
/// and a missing `cwd` has no directory; both are shown as `?` rather than
/// as nothing, because "one session, somewhere" is still worth saying.
fn label_for(cwd: Option<&str>) -> String {
    cwd.map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .and_then(|cwd| {
            Path::new(cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "?".to_owned())
}

/// Tell the daemon about this session, and never mind if it will not listen.
///
/// SPEC §9.3 gives the hook a hundred milliseconds, and the daemon may not be
/// running at all — `hivemind hook check` is installed once and the daemon is
/// stopped and started freely. Every failure here is silence: a hook that
/// printed an error, or waited, would interrupt somebody's work to report
/// something they did not ask about.
async fn tell_the_daemon(api: &str, event: &HookEvent) {
    let client = crate::client::Client::new(api).impatient(HOOK_BUDGET);
    let path = format!("/api/v1/sessions/{}", event.session_id);

    let _ = if event.ending {
        client.delete(&path).await
    } else {
        client
            .post_empty_body(&path, &serde_json::json!({ "label": event.label }))
            .await
    };
}

/// What a hook may spend talking to the daemon (SPEC §9.3).
///
/// The budget for the whole hook is 100 ms. This is most of it, because the
/// rest is an `index.db` read on the same machine — and a loopback request
/// that has not finished in 80 ms is one the daemon is not going to answer.
const HOOK_BUDGET: std::time::Duration = std::time::Duration::from_millis(80);

pub(crate) async fn hook_check(home: Option<&Path>, api: &str) {
    // The session register first, because `SessionEnd` has nothing else to do
    // here and the rest of this is about unread mail.
    let event = hook_event(&read_hook_payload());
    if let Some(event) = &event {
        tell_the_daemon(api, event).await;
        if event.ending {
            // The session is going away. Whatever is unread will still be
            // unread at the next one, and a summary printed into a transcript
            // nobody will read again is noise.
            return;
        }
    }

    let Ok(home) = paths::home(home) else {
        // A hook that fails is a hook that interrupts someone's work. Anything
        // unexpected here means "say nothing", never "print an error".
        return;
    };

    let Ok(index) = Index::open(&home.join("index.db")) else {
        return;
    };
    let Ok(summaries) = index.search(&hivemind_core::index::Query {
        mailbox: Some(hivemind_core::store::Mailbox::New),
        unread_only: true,
        limit: Some(3),
        ..Default::default()
    }) else {
        return;
    };

    if summaries.is_empty() {
        return;
    }

    // The preview shows at most three; the count is the real total, and falls
    // back to what we can see if the count query fails.
    let visible = summaries.len() as u64;
    let total = index.unread_count().unwrap_or(visible);
    let preview: Vec<String> = summaries
        .iter()
        .map(|s| format!("{}: {:?}", short_node(&s.from.to_string()), s.subject))
        .collect();

    // One line, no colour: this goes into a transcript, not a terminal.
    println!(
        "hivemind: {} — {}",
        unread_phrase(total),
        preview.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_names_its_session_and_what_it_is_working_on() {
        let event = hook_event(
            r#"{"session_id":"01JXT","cwd":"/Users/ana/repos/hivemind",
                "hook_event_name":"UserPromptSubmit"}"#,
        )
        .expect("a usable event");

        assert_eq!(event.session_id, "01JXT");
        assert_eq!(event.label, "hivemind", "the basename, not the whole path");
        assert!(!event.ending);
    }

    #[test]
    fn session_end_is_the_one_that_closes_a_session() {
        let event = hook_event(
            r#"{"session_id":"01JXT","cwd":"/Users/ana/repos/hivemind",
                "hook_event_name":"SessionEnd"}"#,
        )
        .expect("a usable event");
        assert!(event.ending);
    }

    #[test]
    fn a_payload_with_fields_we_have_never_seen_is_still_usable() {
        // Claude Code owns this document and sends more than hivemind reads.
        // A hook that refused to parse an unfamiliar one would break on the
        // next version of something it does not control.
        let event = hook_event(
            r#"{"session_id":"01JXT","cwd":"/tmp/work","hook_event_name":"SessionStart",
                "transcript_path":"/x","source":"startup","something_new":{"a":1}}"#,
        )
        .expect("a usable event");
        assert_eq!(event.session_id, "01JXT");
        assert_eq!(event.label, "work");
    }

    #[test]
    fn nothing_usable_is_nothing_rather_than_a_guess() {
        // Each of these is a hook run by hand, or by something that is not
        // Claude Code. None of them is a session, and inventing an id would
        // put a phantom in somebody else's peer list.
        assert_eq!(hook_event(""), None, "no payload at all");
        assert_eq!(hook_event("not json"), None);
        assert_eq!(hook_event("{}"), None, "no session id");
        assert_eq!(hook_event(r#"{"session_id":"   "}"#), None, "a blank one");
        assert_eq!(hook_event(r#"{"session_id":42}"#), None, "not a string");
    }

    #[test]
    fn a_session_with_nowhere_to_call_home_is_still_a_session() {
        // A `cwd` that is the filesystem root has no basename, and a hook
        // payload without one is possible. "One session, somewhere" is worth
        // more than dropping the registration.
        for payload in [
            r#"{"session_id":"01JXT"}"#,
            r#"{"session_id":"01JXT","cwd":"/"}"#,
            r#"{"session_id":"01JXT","cwd":"  "}"#,
        ] {
            let event = hook_event(payload).expect("still a session");
            assert_eq!(event.label, "?", "{payload}");
        }
    }

    #[test]
    fn a_trailing_slash_does_not_cost_the_label() {
        // `cwd` is normally clean, but "/Users/ana/repos/hivemind/" naming a
        // session "?" would be a silly way to lose the useful half.
        let event = hook_event(r#"{"session_id":"01JXT","cwd":"/Users/ana/repos/hivemind/"}"#)
            .expect("a usable event");
        assert_eq!(event.label, "hivemind");
    }
}
