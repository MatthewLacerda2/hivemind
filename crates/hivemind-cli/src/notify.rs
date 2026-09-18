//! Desktop notifications when mail arrives (SPEC §9.4).
//!
//! Best effort, always. A notification that fails must never affect delivery:
//! the mail is already on disk by the time we get here, and a missing banner is
//! a cosmetic problem.

use std::sync::Arc;

use hivemind_api::service::{Event, MailService};

/// Watch for arriving mail and post a notification for each one.
///
/// Runs until the daemon shuts down.
pub(crate) async fn watch(service: Arc<MailService>, enabled: bool) {
    if !enabled {
        return;
    }

    let mut events = service.subscribe();
    loop {
        match events.recv().await {
            Ok(Event::MessageReceived { id }) => {
                let subject = service
                    .get(id)
                    .map_or_else(|_| "new message".to_owned(), |(_, message)| message.subject);
                post("hivemind", &subject);
            }
            // Other events are not worth a banner; and a notifier that fell
            // behind has missed banners it cannot usefully show late.
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Post one notification, or quietly give up.
fn post(title: &str, body: &str) {
    let result = platform_command(title, body).map(|mut c| c.status());

    match result {
        Some(Ok(_)) | None => {}
        Some(Err(error)) => {
            // Logged, never surfaced: the user already has the mail.
            tracing::debug!(%error, "could not post a desktop notification");
        }
    }
}

/// The command that shows a banner on this platform.
///
/// `None` on a platform with no notifier, which is not an error: SPEC §1 ships
/// macOS and Linux, and everywhere else simply shows nothing.
#[allow(
    clippy::unnecessary_wraps,
    reason = "None on platforms with no notifier"
)]
fn platform_command(title: &str, body: &str) -> Option<std::process::Command> {
    // Both arguments end up inside a quoted string, so a subject containing a
    // quote or a backslash would otherwise end the string early. This is a
    // notification, but it is still someone else's text reaching a shell-ish
    // interpreter.
    let safe_title = escape(title);
    let safe_body = escape(body);

    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("osascript");
        command.arg("-e").arg(format!(
            "display notification \"{safe_body}\" with title \"{safe_title}\""
        ));
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        Some(command)
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = std::process::Command::new("notify-send");
        command.arg(safe_title).arg(safe_body);
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        Some(command)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (safe_title, safe_body);
        None
    }
}

/// Neutralise the characters that would end an `AppleScript` string early.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subject_containing_a_quote_cannot_end_the_script_string() {
        let escaped = escape(r#"he said "ship it""#);
        assert_eq!(escaped, r#"he said \"ship it\""#);
    }

    #[test]
    fn a_backslash_is_escaped_before_the_quotes_are() {
        // Escaping quotes first would turn \" into \\" and reopen the string.
        assert_eq!(escape(r#"a\"b"#), r#"a\\\"b"#);
    }

    #[test]
    fn newlines_become_spaces_so_one_subject_is_one_line() {
        assert_eq!(escape("two\nlines"), "two lines");
        assert_eq!(escape("crlf\r\nhere"), "crlf  here");
    }

    #[test]
    fn an_ordinary_subject_is_left_alone() {
        assert_eq!(escape("dashboard PR"), "dashboard PR");
    }

    #[test]
    fn posting_a_notification_never_panics_even_with_hostile_text() {
        // It may or may not show a banner depending on the machine; what it
        // must not do is fail.
        post("hivemind", r#"'; rm -rf /; echo ""#);
    }
}
