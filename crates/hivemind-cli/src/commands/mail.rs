//! The mail verbs: `send`, `inbox`, `chats`, `sent`, `read`, `reply` and
//! `thread`.
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
use crate::events;

use super::short_node;

mod delivery;

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
    thread_id: String,
    from: String,
    subject: String,
    sender_kind: String,
    sent_at: chrono::DateTime<chrono::Utc>,
    unread: bool,
    mailbox: String,
    attachment_names: Vec<String>,
    /// How far it has got with its recipients, for a message this machine
    /// sent. Absent for mail that arrived here.
    #[serde(default)]
    delivery: Option<delivery::Delivery>,
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

/// One conversation, as the local API reports it.
#[derive(Debug, Deserialize)]
struct Conversation {
    thread_id: String,
    subject: String,
    participants: Vec<String>,
    messages: usize,
    unread: usize,
    last_from: String,
    last_sender_kind: String,
    last_at: chrono::DateTime<chrono::Utc>,
}

/// List the conversations, the one that moved last first (SPEC §10).
///
/// A conversation is a thread, so this is `hivemind thread` seen from the
/// outside: `inbox` shows six messages about one subject and three about
/// another, with the same machine, as nine mixed lines (#43).
pub(crate) async fn chats(api: &str, with: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let mut path = format!("/api/v1/threads?limit={limit}");
    if let Some(with) = with {
        // A node id, whole or short: both are URL-safe as they are printed,
        // and the daemon refuses a machine it does not know rather than
        // answering with an empty list.
        path.push_str("&with=");
        path.push_str(with.trim());
    }

    let chats: Vec<Conversation> = Client::new(api).get(&path).await?;
    show_chats(&chats, json)
}

/// Print the conversation list, the one way this CLI prints one.
fn show_chats(chats: &[Conversation], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &chats
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "thread_id": c.thread_id, "subject": c.subject,
                            "participants": c.participants, "messages": c.messages,
                            "unread": c.unread, "last_from": c.last_from,
                            "last_sender_kind": c.last_sender_kind, "last_at": c.last_at,
                        })
                    })
                    .collect::<Vec<_>>()
            )?
        );
        return Ok(());
    }

    if chats.is_empty() {
        println!("{}", "no conversations".dimmed());
        return Ok(());
    }

    for chat in chats {
        let marker = if chat.unread > 0 { "●" } else { " " };
        // The badge says whether the last word was a person's or a Claude's,
        // which in a conversation is the thing you want before you answer it.
        let badge = match chat.last_sender_kind.as_str() {
            "agent" => "[agent]".magenta(),
            _ => "[human]".cyan(),
        };
        let with: Vec<String> = chat.participants.iter().map(|id| short_node(id)).collect();

        println!(
            "{marker} {} {}",
            short_id(&chat.thread_id).dimmed(),
            chat.subject.bold()
        );
        println!(
            "    {} · {badge} {} · {}",
            with.join(", "),
            chat.last_at.format("%Y-%m-%d %H:%M").dimmed(),
            counted(chat.messages, chat.unread).dimmed()
        );
    }

    // The two doors out of this list, said once rather than per row. Both take
    // the id printed above, which is the conversation's.
    println!();
    println!(
        "{}",
        "read one with `hivemind thread <id>` · answer it with `hivemind reply <id>`".dimmed()
    );
    Ok(())
}

/// How much is in a conversation, and how much of it is new.
fn counted(messages: usize, unread: usize) -> String {
    let messages = count_phrase(messages);
    if unread == 0 {
        messages
    } else {
        format!("{messages}, {unread} unread")
    }
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

    show(&summaries, json)
}

/// Print a page of summaries, the one way this CLI prints a list of mail.
///
/// `inbox`, `sent` and `wait` all go through here, which is what makes "`wait`
/// prints it as `inbox` prints it" true rather than intended (#40).
fn show(summaries: &[Summary], json: bool) -> Result<()> {
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
                            "delivery": s.delivery.as_ref().map(|d| serde_json::json!({
                                "state": d.state, "recipients": d.recipients,
                                "delivered": d.delivered, "read": d.read,
                            })),
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

    for summary in summaries {
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

        // A row this machine sent is the answer to "I sent it, did it
        // arrive?", and printing it like any other would answer yes (#26).
        // The mark says which recipients have it rather than which box it is
        // in, because a message everybody has is still one somebody asks
        // about (#31).
        let state = summary
            .delivery
            .as_ref()
            .map(|d| format!(" {}", delivery::row_coloured(d)))
            .unwrap_or_default();

        println!(
            "{marker} {} {badge} {}{attachments}",
            short_id(&summary.id).dimmed(),
            summary.subject.bold(),
        );
        println!(
            "    {} {}{state}",
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
    /// One entry per recipient, for a message this machine sent.
    #[serde(default)]
    recipients: Vec<delivery::Recipient>,
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

    // What each recipient did with it, for something this machine sent. The
    // question #31 is about — "did it arrive, and to whom" — is asked of one
    // message, and this is where it is answered.
    delivery::block(&message.recipients);

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
        "recipients": message.recipients.iter().map(|r| serde_json::json!({
            "node": r.node, "state": r.state, "delivered_at": r.delivered_at,
            "read_at": r.read_at, "attempts": r.attempts, "last_error": r.last_error,
        })).collect::<Vec<_>>(),
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

/// How a wait ended, which is the difference between exiting 0 and exiting 3.
///
/// A bool would do and is exactly what must not be used: the whole point of
/// this command is that "nothing arrived" and "something did" never wear the
/// same face, and that starts with the two of them having names (#40).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// Mail matching what was asked for is in the box, and has been printed.
    Arrived,
    /// The timeout passed first.
    TimedOut,
}

/// What a wait is for, once the arguments have been resolved (SPEC §10).
///
/// Pure, and matched against here rather than in the API's query string, for
/// two reasons: `--from` may name a person who runs several machines, which
/// `?from=` cannot express, and the decision "is this the message I am waiting
/// for" is the one thing in this command that must be testable without a clock
/// or a socket.
#[derive(Debug, Default)]
struct Want {
    /// The node ids `--from` named, if it was given. Empty is not this:
    /// nothing to wait for is refused when the argument is resolved.
    from: Option<Vec<String>>,
    /// The thread `--thread` named, as its full id.
    thread: Option<String>,
}

impl Want {
    /// Is this the message we are waiting for?
    fn matches(&self, summary: &Summary) -> bool {
        self.from
            .as_ref()
            .is_none_or(|ids| ids.iter().any(|id| id == &summary.from))
            && self
                .thread
                .as_ref()
                .is_none_or(|id| id == &summary.thread_id)
    }
}

/// A node this machine can name: a peer, or itself.
#[derive(Debug, Deserialize)]
struct Named {
    id: String,
    short_id: String,
    name: String,
    owner: Option<String>,
}

impl Named {
    /// Is `typed` one of this node's names?
    ///
    /// The four spellings somebody has to hand: the full node id, the short id
    /// `hivemind peers` prints, the machine's name, and its owner's. The same
    /// four `send` accepts as a recipient, because "wait for a reply from the
    /// machine I just sent to" should not need a different spelling.
    fn answers_to(&self, typed: &str) -> bool {
        let spellings = [
            Some(self.id.as_str()),
            Some(self.short_id.as_str()),
            Some(self.name.as_str()),
            self.owner.as_deref(),
        ];
        spellings
            .into_iter()
            .flatten()
            .any(|spelling| spelling.eq_ignore_ascii_case(typed))
    }
}

/// Which nodes `typed` means. Several, when it is a person with two machines.
fn senders(typed: &str, known: &[Named]) -> Vec<String> {
    // An empty argument names nothing rather than everything: `--from ""` is a
    // shell variable that did not expand, and answering it with the whole
    // address book would wait for the wrong message and say it was the one.
    if typed.is_empty() {
        return Vec::new();
    }
    known
        .iter()
        .filter(|node| node.answers_to(typed))
        .map(|node| node.id.clone())
        .collect()
}

/// A timeout as somebody types it: `30s`, `5m`, `2h`, or bare seconds.
///
/// Hand-rolled rather than `humantime`, which is one crate for one argument
/// (CLAUDE.md, dependencies). The rule is deliberately narrow: a spelling this
/// does not understand is refused, because a `--timeout 5` that silently meant
/// five seconds when five minutes was intended is the failure this command is
/// here to prevent, one level up.
fn duration(typed: &str) -> Result<std::time::Duration> {
    let refusal =
        || anyhow::anyhow!("`{typed}` is not a length of time — try `30s`, `5m`, `2h`, or seconds");

    let (digits, scale) = match typed.strip_suffix(['s', 'm', 'h']) {
        // The digits come back without the unit, and `typed` still ends
        // with it, which is where the scale is read from.
        Some(digits) => (
            digits,
            match typed.as_bytes().last() {
                Some(b'm') => 60,
                Some(b'h') => 60 * 60,
                _ => 1,
            },
        ),
        None => (typed, 1),
    };

    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refusal());
    }
    let seconds: u64 = digits.parse().map_err(|_| refusal())?;
    seconds
        .checked_mul(scale)
        .map(std::time::Duration::from_secs)
        .ok_or_else(refusal)
}

/// Block until mail arrives, and print it as `inbox` does (SPEC §10).
///
/// # Errors
/// When the daemon is not there, stops answering mid-wait, or cannot resolve
/// what `--from` or `--thread` named.
pub(crate) async fn wait(
    api: &str,
    from: Option<&str>,
    thread: Option<&str>,
    timeout: Option<&str>,
    json: bool,
) -> Result<Waited> {
    let limit = timeout.map(duration).transpose()?;
    let client = Client::new(api);
    let want = resolve(&client, from, thread).await?;

    // Subscribe *before* looking in the box. A message that arrives between
    // the two is otherwise reported by neither: it is not in the answer to the
    // question already asked, and its event went to a stream nobody had opened
    // yet. That ordering is most of what this command is for — the loop it
    // replaces was written four times by hand and the first one could have
    // waited four hours with the mail already sitting there (#40).
    let mut events = events::Stream::open(api).await?;

    let watching = watch(&client, &mut events, &want, json);
    let Some(limit) = limit else {
        watching.await?;
        return Ok(Waited::Arrived);
    };

    let Ok(result) = tokio::time::timeout(limit, watching).await else {
        // On stderr, and never on stdout: a script reading `--json` must not be
        // handed prose, and the exit status is the answer it is reading.
        eprintln!(
            "nothing arrived within {}",
            timeout.unwrap_or_default().dimmed()
        );
        return Ok(Waited::TimedOut);
    };
    result.map(|()| Waited::Arrived)
}

/// Look in the box, then wait for the next arrival and look again.
async fn watch(
    client: &Client,
    events: &mut events::Stream,
    want: &Want,
    json: bool,
) -> Result<()> {
    loop {
        let waiting = wanted_unread(client, want).await?;
        if !waiting.is_empty() {
            show(&waiting, json)?;
            return Ok(());
        }

        // Only mail arriving ends a wait. The stream also carries deliveries,
        // reads and peers coming and going, and waking up for those would put
        // this command back to polling with extra steps. The daemon going away
        // is an error rather than a longer wait, which is `next`'s business.
        while events.next().await?.name != "message.received" {}
    }
}

/// The unread mail that matches, newest first.
///
/// `box=new` is the unread box by construction, so this is the whole of what a
/// wait can be about. One page of it: a matching message that is already
/// hundreds of messages old is in a box nobody is reading, and anything that
/// arrives while waiting is by definition on the first page.
async fn wanted_unread(client: &Client, want: &Want) -> Result<Vec<Summary>> {
    let mut summaries: Vec<Summary> = client.get("/api/v1/messages?box=new&limit=200").await?;
    summaries.retain(|summary| want.matches(summary));
    summaries.sort_by_key(|s| std::cmp::Reverse(s.sent_at));
    Ok(summaries)
}

/// Turn `--from` and `--thread` into ids, refusing what names nothing.
///
/// Both of these are resolved before the wait rather than during it, because
/// the alternative is waiting for ever on a typo — and a wait that can never
/// end is exactly the failure this command exists to remove (#40).
async fn resolve(client: &Client, from: Option<&str>, thread: Option<&str>) -> Result<Want> {
    let mut want = Want::default();

    if let Some(typed) = from {
        let mut known: Vec<Named> = client.get("/api/v1/peers").await?;
        // This machine among them: mail sent to `everyone` arrives here too,
        // and waiting for it by name should work like waiting for anyone else.
        known.push(client.get("/api/v1/me").await?);

        let ids = senders(typed, &known);
        anyhow::ensure!(
            !ids.is_empty(),
            "no machine or person here is called `{typed}` — `hivemind peers` lists who this one knows"
        );
        want.from = Some(ids);
    }

    if let Some(typed) = thread {
        let summaries = thread_summaries(client, typed)
            .await
            .with_context(|| format!("cannot wait on a thread `{typed}` does not name"))?;
        let first = summaries
            .first()
            .with_context(|| format!("no thread here has a message `{typed}`"))?;
        want.thread = Some(first.thread_id.clone());
    }

    Ok(want)
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
    fn a_conversation_says_how_much_of_it_is_new_only_when_some_of_it_is() {
        // "6 messages, 0 unread" is a line that makes somebody look twice at a
        // conversation there is nothing new in.
        assert_eq!(counted(6, 2), "6 messages, 2 unread");
        assert_eq!(counted(6, 0), "6 messages");
        assert_eq!(counted(1, 1), "1 message, 1 unread");
    }

    /// A summary with the two fields a wait judges on, and the rest plausible.
    fn summary(from: &str, thread_id: &str) -> Summary {
        Summary {
            id: "01JXT21Q00041061050R3GG28A".to_owned(),
            thread_id: thread_id.to_owned(),
            from: from.to_owned(),
            subject: "anything".to_owned(),
            sender_kind: "human".to_owned(),
            sent_at: chrono::Utc::now(),
            unread: true,
            mailbox: "new".to_owned(),
            attachment_names: Vec::new(),
            delivery: None,
        }
    }

    fn named(id: &str, short_id: &str, name: &str, owner: Option<&str>) -> Named {
        Named {
            id: id.to_owned(),
            short_id: short_id.to_owned(),
            name: name.to_owned(),
            owner: owner.map(str::to_owned),
        }
    }

    #[test]
    fn an_unfiltered_wait_takes_the_first_thing_that_arrives() {
        assert!(Want::default().matches(&summary("hm1:w2mq-xor2", "01AAA")));
    }

    #[test]
    fn a_wait_for_one_sender_ignores_everybody_else() {
        let want = Want {
            from: Some(vec!["hm1:w2mq-xor2".to_owned()]),
            thread: None,
        };
        assert!(want.matches(&summary("hm1:w2mq-xor2", "01AAA")));
        assert!(!want.matches(&summary("hm1:zzzz-nope", "01AAA")));
    }

    #[test]
    fn a_wait_for_a_person_with_two_machines_takes_either_of_them() {
        let want = Want {
            from: Some(vec!["hm1:one".to_owned(), "hm1:two".to_owned()]),
            thread: None,
        };
        assert!(want.matches(&summary("hm1:one", "01AAA")));
        assert!(want.matches(&summary("hm1:two", "01AAA")));
        assert!(!want.matches(&summary("hm1:three", "01AAA")));
    }

    #[test]
    fn a_wait_for_one_thread_ignores_other_conversations() {
        let want = Want {
            from: None,
            thread: Some("01AAA".to_owned()),
        };
        assert!(want.matches(&summary("hm1:anyone", "01AAA")));
        assert!(!want.matches(&summary("hm1:anyone", "01BBB")));
    }

    #[test]
    fn both_filters_together_have_to_both_hold() {
        let want = Want {
            from: Some(vec!["hm1:one".to_owned()]),
            thread: Some("01AAA".to_owned()),
        };
        assert!(want.matches(&summary("hm1:one", "01AAA")));
        assert!(!want.matches(&summary("hm1:one", "01BBB")));
        assert!(!want.matches(&summary("hm1:two", "01AAA")));
    }

    #[test]
    fn a_sender_is_named_four_ways() {
        let known = vec![named("hm1:w2mq-xor2-seiv", "w2mq", "arch", Some("matthew"))];
        for typed in ["hm1:w2mq-xor2-seiv", "w2mq", "arch", "matthew", "MATTHEW"] {
            assert_eq!(
                senders(typed, &known),
                vec!["hm1:w2mq-xor2-seiv".to_owned()],
                "{typed} should have named that node"
            );
        }
    }

    #[test]
    fn a_person_who_runs_two_machines_is_both_of_them() {
        let known = vec![
            named("hm1:one", "one", "arch", Some("matthew")),
            named("hm1:two", "two", "mac", Some("matthew")),
            named("hm1:three", "three", "pi", Some("somebody else")),
        ];
        assert_eq!(
            senders("matthew", &known),
            vec!["hm1:one".to_owned(), "hm1:two".to_owned()]
        );
    }

    #[test]
    fn a_name_nobody_here_has_names_nothing() {
        // Which is what turns a typo into a refusal rather than a wait that
        // can never end.
        let known = vec![named("hm1:one", "one", "arch", Some("matthew"))];
        assert!(senders("nobody", &known).is_empty());
        assert!(senders("", &known).is_empty());
    }

    #[test]
    fn a_timeout_is_read_in_seconds_minutes_or_hours() {
        use std::time::Duration;
        assert_eq!(duration("30s").expect("30s"), Duration::from_secs(30));
        assert_eq!(duration("5m").expect("5m"), Duration::from_mins(5));
        assert_eq!(duration("2h").expect("2h"), Duration::from_hours(2));
        assert_eq!(
            duration("45").expect("bare seconds"),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn a_timeout_nobody_can_read_is_refused_rather_than_guessed() {
        for typed in ["", "soon", "5 m", "1.5m", "-3s", "5d", "m", "5min"] {
            let complaint = duration(typed)
                .expect_err(&format!("`{typed}` should not have parsed"))
                .to_string();
            assert!(
                complaint.contains("30s"),
                "the refusal should say what it does take: {complaint}"
            );
        }
    }

    #[test]
    fn a_short_id_copes_with_something_shorter_than_it_expects() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }
}
