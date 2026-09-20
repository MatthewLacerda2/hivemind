//! The mail verbs: `send`, `inbox`, `sent`, `read`, `reply` and `thread`.
//!
//! Each one is a call to the loopback API and a way of printing what came
//! back, because the CLI is an HTTP client like any other (SPEC §10). They are
//! together because they share the shapes the API answers with and the way a
//! message is put on a terminal.

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::body;
use crate::client::Client;
use crate::colour::Paint as _;

use super::short_node;

#[derive(Debug, Deserialize)]
struct Accepted {
    id: String,
    duplicate_of: Option<String>,
}

/// Say what was queued, and whether it has just been queued before (#33).
///
/// A warning rather than a refusal: sending the same thing again is a real
/// thing to want, and the daemon has already accepted this one. What it buys is
/// the person who pressed send twice finding out now rather than the person at
/// the other end finding out tomorrow.
fn report(accepted: &Accepted) {
    println!("{} {}", "queued".green(), accepted.id.dimmed());
    if let Some(previous) = &accepted.duplicate_of {
        println!(
            "{} {} {}",
            "warning:".yellow(),
            "the same message went out moments ago —".yellow(),
            previous.dimmed()
        );
    }
}

/// Send a message (SPEC §10).
pub(crate) async fn send(
    api: &str,
    to: &[String],
    subject: &str,
    attach: &[std::path::PathBuf],
    body: Option<&str>,
    trailing_body: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        !to.is_empty(),
        "who is this for? pass a node id, an owner name, or `everyone`"
    );
    let body = body::resolve(body, trailing_body)?;

    // Absolute, because the daemon reads them and it is not in this directory.
    let attachments = attach
        .iter()
        .map(|path| {
            std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .with_context(|| format!("cannot read {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;

    let accepted: Accepted = Client::new(api)
        .post(
            "/api/v1/messages",
            &serde_json::json!({
                "to": to,
                "subject": subject,
                "body": body,
                "attachments": attachments,
            }),
        )
        .await?;

    // SPEC §8: accepted, not delivered. Saying "sent" would be a promise the
    // outbox has not kept yet.
    report(&accepted);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Summary {
    id: String,
    from: String,
    subject: String,
    sender_kind: String,
    sent_at: chrono::DateTime<chrono::Utc>,
    unread: bool,
    mailbox: String,
    attachment_names: Vec<String>,
}

/// Which of the four boxes to list (SPEC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum BoxArg {
    /// Arrived here and not read yet.
    New,
    /// Arrived here and already read.
    Cur,
    /// Written here and still waiting for a recipient to take it.
    Out,
    /// Written here and delivered to every recipient.
    Sent,
}

impl BoxArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Cur => "cur",
            Self::Out => "out",
            Self::Sent => "sent",
        }
    }
}

/// List mail (SPEC §10).
///
/// No `--box` means both boxes of what arrived, which is what the web UI calls
/// the inbox and what somebody asking for theirs means.
pub(crate) async fn inbox(
    api: &str,
    chosen: Option<BoxArg>,
    unread_only: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    // Only `new/` holds unread mail, so `--unread --box sent` asks for mail
    // that is read and unread at once. Refusing beats answering nothing: an
    // empty list looks like an empty box, and that is precisely how a query
    // that made no sense was read as `out/` being broken (#28).
    if let Some(chosen) = chosen.filter(|_| unread_only) {
        anyhow::ensure!(
            chosen == BoxArg::New,
            "nothing in `{}` is unread — drop `--unread`, or drop `--box`",
            chosen.as_str()
        );
    }

    let boxes = chosen.map_or_else(|| vec![BoxArg::New, BoxArg::Cur], |one| vec![one]);
    listing(api, &boxes, unread_only, limit, json).await
}

/// List what this machine sent, still going out or gone (SPEC §10).
///
/// `out/` with `sent/`, the pair the web UI's Sent view shows: a message still
/// being delivered is one you sent, and hiding it until the last recipient
/// takes it would make a peer being off look like the message vanishing.
pub(crate) async fn sent(api: &str, limit: usize, json: bool) -> Result<()> {
    listing(api, &[BoxArg::Out, BoxArg::Sent], false, limit, json).await
}

/// Print one page of the boxes asked for, newest first across all of them.
async fn listing(
    api: &str,
    boxes: &[BoxArg],
    unread_only: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let client = Client::new(api);
    let mut summaries: Vec<Summary> = Vec::new();
    // One request per box, because the API filters one at a time (SPEC §7.1)
    // and a message somebody sent to themselves genuinely sits in two of them
    // — once as something that arrived, once as something they sent.
    for chosen in boxes {
        let path = format!(
            "/api/v1/messages?box={}&limit={limit}{}",
            chosen.as_str(),
            if unread_only { "&unread=true" } else { "" }
        );
        summaries.extend(client.get::<Vec<Summary>>(&path).await?);
    }
    // Newest first across the boxes, then capped again: a page of each merged
    // is more than a page.
    summaries.sort_by_key(|s| std::cmp::Reverse(s.sent_at));
    summaries.truncate(limit);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &summaries
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id, "from": s.from, "subject": s.subject,
                            "sender_kind": s.sender_kind, "sent_at": s.sent_at,
                            "unread": s.unread, "mailbox": s.mailbox,
                            "attachment_names": s.attachment_names,
                        })
                    })
                    .collect::<Vec<_>>()
            )?
        );
        return Ok(());
    }

    if summaries.is_empty() {
        println!("{}", "no mail".dimmed());
        return Ok(());
    }

    for summary in &summaries {
        let marker = if summary.unread { "●" } else { " " };
        // The badge is the point of sender_kind: you should be able to see at a
        // glance whether a person wrote this or a Claude did (SPEC §11).
        let badge = match summary.sender_kind.as_str() {
            "agent" => "[agent]".magenta(),
            _ => "[human]".cyan(),
        };
        let attachments = if summary.attachment_names.is_empty() {
            String::new()
        } else {
            format!(" 📎{}", summary.attachment_names.len())
        };

        // A row in `out/` is the answer to "I sent it, did it arrive?", and
        // printing it like any other would answer yes (#26).
        let waiting = if summary.mailbox == "out" {
            " waiting to be delivered".yellow()
        } else {
            String::new()
        };

        println!(
            "{marker} {} {badge} {}{attachments}",
            short_id(&summary.id).dimmed(),
            summary.subject.bold(),
        );
        println!(
            "    {} {}{waiting}",
            summary.sent_at.format("%Y-%m-%d %H:%M").dimmed(),
            short_node(&summary.from).dimmed()
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    id: String,
    thread_id: String,
    from: String,
    subject: String,
    body: String,
    sender_kind: String,
    sent_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

/// One attachment, as the local API reports it.
#[derive(Debug, Deserialize, serde::Serialize)]
struct Attachment {
    name: String,
    size: u64,
    sha256: String,
    mime: String,
    inline: bool,
    cached: bool,
}

/// Read one message and mark it read (SPEC §10).
pub(crate) async fn read(api: &str, id: &str, json: bool) -> Result<()> {
    let client = Client::new(api);
    let message: MessageBody = client.get(&format!("/api/v1/messages/{id}")).await?;
    // Reading is what marks it read; the API keeps the two separate so the web
    // UI can preview without changing state.
    client
        .post_empty(&format!("/api/v1/messages/{id}/read"))
        .await?;

    // What else was said on this subject. A message read on its own is how a
    // three-day conversation ends up reconstructed from memory (#34).
    let others = thread_summaries(&client, &message.thread_id)
        .await?
        .len()
        .saturating_sub(1);

    if json {
        let mut reported = as_json(&message);
        reported["others_in_thread"] = others.into();
        println!("{}", serde_json::to_string_pretty(&reported)?);
        return Ok(());
    }

    println!("{}", message.subject.bold());
    println!(
        "{} {} · {} · {}",
        "from".dimmed(),
        short_node(&message.from),
        message.sender_kind,
        message.sent_at.format("%Y-%m-%d %H:%M")
    );
    println!();
    println!("{}", message.body);

    if !message.attachments.is_empty() {
        println!();
        println!("{}", "attachments".dimmed());
        for attachment in &message.attachments {
            // "on disk" vs "fetch on read" is the difference between opening
            // it now and waiting for the sender's laptop to be awake.
            let state = if attachment.cached {
                "on disk".green()
            } else {
                "fetch on read".yellow()
            };
            println!(
                "  {}  {}  {}  {}",
                attachment.name.bold(),
                human_size(attachment.size),
                attachment.mime.dimmed(),
                state
            );
            println!("    {}", attachment.sha256.dimmed());
        }
    }

    if others > 0 {
        println!();
        println!(
            "{}",
            format!(
                "{others} more in this thread · hivemind thread {}",
                short_id(&message.thread_id)
            )
            .dimmed()
        );
    }
    Ok(())
}

/// Read a whole conversation, oldest first (SPEC §10).
///
/// Takes the id of any message in it, which is the point: the id somebody has
/// to hand is the one they were just reading, and nobody knows by heart which
/// message was first (#34).
pub(crate) async fn thread(api: &str, id: &str, json: bool) -> Result<()> {
    let client = Client::new(api);
    let summaries = thread_summaries(&client, id).await?;

    // The thread endpoint answers with summaries, and a conversation without
    // the bodies is the inbox again — so each message is fetched in full.
    let mut messages = Vec::with_capacity(summaries.len());
    for summary in &summaries {
        messages.push(
            client
                .get::<MessageBody>(&format!("/api/v1/messages/{}", summary.id))
                .await?,
        );
        // Reading a conversation is reading the messages in it, which is what
        // the web UI's thread view already decided (SPEC §11). Only what
        // arrived and is unread: `new` is the one box this moves anything out
        // of, and asking for the others would be a 404 on our own sent mail.
        if summary.mailbox == "new" {
            client
                .post_empty(&format!("/api/v1/messages/{}/read", summary.id))
                .await?;
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&messages.iter().map(as_json).collect::<Vec<_>>())?
        );
        return Ok(());
    }

    // The root's subject: the replies are all `Re:` it, and a conversation has
    // one subject by construction.
    let Some(first) = messages.first() else {
        return Ok(());
    };
    println!("{}", first.subject.bold());
    println!(
        "{}",
        format!(
            "{} · thread {}",
            count_phrase(messages.len()),
            short_id(&first.thread_id)
        )
        .dimmed()
    );

    for message in &messages {
        println!();
        // `read`'s own line, with the message id on the end: in a conversation
        // that id is what a `reply` to one particular message needs. Who sent
        // it is shown exactly as `inbox` and `read` show it (#42 is where that
        // changes, for all three at once).
        println!(
            "{} {} · {} · {} · {}",
            "from".dimmed(),
            short_node(&message.from),
            message.sender_kind,
            message.sent_at.format("%Y-%m-%d %H:%M"),
            short_id(&message.id).dimmed()
        );
        println!("{}", message.body);
        if !message.attachments.is_empty() {
            let names: Vec<&str> = message
                .attachments
                .iter()
                .map(|a| a.name.as_str())
                .collect();
            // Names only: `hivemind read` is where the sizes and the shas are.
            println!("{}", format!("📎 {}", names.join(", ")).dimmed());
        }
    }
    Ok(())
}

/// The conversation `id` belongs to, oldest first.
async fn thread_summaries(client: &Client, id: &str) -> Result<Vec<Summary>> {
    client.get(&format!("/api/v1/threads/{id}")).await
}

/// One message as `--json` reports it, the same shape from `read` and `thread`.
fn as_json(message: &MessageBody) -> serde_json::Value {
    serde_json::json!({
        "id": message.id, "thread_id": message.thread_id, "from": message.from,
        "subject": message.subject, "body": message.body,
        "sender_kind": message.sender_kind, "sent_at": message.sent_at,
        "attachments": message.attachments,
    })
}

/// How long the conversation is, without a stray plural.
fn count_phrase(messages: usize) -> String {
    match messages {
        1 => "1 message".to_owned(),
        n => format!("{n} messages"),
    }
}

/// Reply to a message (SPEC §10).
pub(crate) async fn reply(
    api: &str,
    id: &str,
    body: Option<&str>,
    trailing_body: Option<&str>,
) -> Result<()> {
    let body = body::resolve(body, trailing_body)?;
    let accepted: Accepted = Client::new(api)
        .post(
            &format!("/api/v1/messages/{id}/reply"),
            &serde_json::json!({ "body": body }),
        )
        .await?;
    report(&accepted);
    Ok(())
}

/// ULIDs are long and the first characters are the timestamp, so the tail is
/// what actually distinguishes two messages sent in the same millisecond.
fn short_id(id: &str) -> String {
    id.chars().skip(id.len().saturating_sub(6)).collect()
}

/// A size a person can read at a glance.
fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
        ("B", 1),
    ];

    for (unit, scale) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes} B");
            }
            #[allow(clippy::cast_precision_loss)]
            let value = bytes as f64 / scale as f64;
            return format!("{value:.1} {unit}");
        }
    }
    "0 B".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_id_is_the_distinguishing_tail_not_the_timestamp() {
        assert_eq!(short_id("01JXT21Q00041061050R3GG28A"), "3GG28A");
    }

    #[test]
    fn a_conversation_of_one_is_not_reported_as_1_messages() {
        assert_eq!(count_phrase(1), "1 message");
        assert_eq!(count_phrase(6), "6 messages");
    }

    #[test]
    fn a_short_id_copes_with_something_shorter_than_it_expects() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }
}
