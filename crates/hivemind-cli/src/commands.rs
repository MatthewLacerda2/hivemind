//! Subcommand implementations, one function per verb in SPEC §10.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use hivemind_api::{ApiDoc, MailService};
use hivemind_core::identity::Identity;
use hivemind_core::index::Index;
use hivemind_core::store::MailStore;
use owo_colors::OwoColorize as _;
use serde::Deserialize;

use crate::client::{Client, body_from_arg_or_stdin};
use crate::paths;

/// Print the `OpenAPI` document (SPEC §7).
pub(crate) fn openapi() -> Result<()> {
    println!(
        "{}",
        ApiDoc::to_json().context("could not build the document")?
    );
    Ok(())
}

/// Run the daemon in the foreground (SPEC §10).
pub(crate) async fn daemon(home: Option<&Path>, port: u16) -> Result<()> {
    let home = paths::home(home)?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("could not create {}", home.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HIVEMIND_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let identity = Identity::load_or_create(&home.join("identity"))
        .context("could not load this node's identity")?;
    let service = MailService::open(&home, identity.node_id(), identity.signing_key().clone())
        .context("could not open the mail store")?;

    // SPEC §6.3: loopback only, and it fails closed. The address is not
    // configurable, because "bind somewhere else" is not a preference — it is
    // an unauthenticated API on the network.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;

    tracing::info!(
        node = %identity.node_id(),
        short = %identity.node_id().short(),
        %addr,
        "hivemind is up"
    );
    println!(
        "hivemind {} listening on http://{addr}",
        identity.node_id().short()
    );
    println!("  docs   http://{addr}/docs");
    println!("  node   {}", identity.node_id());

    hivemind_api::serve(listener, Arc::new(service), shutdown())
        .await
        .context("the server stopped unexpectedly")
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

#[derive(Debug, Deserialize)]
struct Me {
    id: String,
    short_id: String,
    version: String,
    unread: u64,
}

/// Is the daemon up, and what does it know (SPEC §10)?
pub(crate) async fn status(api: &str, json: bool) -> Result<()> {
    let me: Me = Client::new(api).get("/api/v1/me").await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": me.id, "short_id": me.short_id, "version": me.version, "unread": me.unread,
            }))?
        );
        return Ok(());
    }

    println!("{} {}", "node".dimmed(), me.short_id.bold());
    println!("{} {}", "  id  ".dimmed(), me.id);
    println!("{} v{}", "  ver ".dimmed(), me.version);
    println!("{} {}", "  mail".dimmed(), unread_phrase(me.unread));
    Ok(())
}

fn unread_phrase(unread: u64) -> String {
    match unread {
        0 => "no unread messages".to_owned(),
        1 => "1 unread message".to_owned(),
        n => format!("{n} unread messages"),
    }
}

#[derive(Debug, Deserialize)]
struct Accepted {
    id: String,
}

/// Send a message (SPEC §10).
pub(crate) async fn send(
    api: &str,
    to: &[String],
    subject: &str,
    body: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        !to.is_empty(),
        "who is this for? pass a node id, an owner name, or `everyone`"
    );
    let body = body_from_arg_or_stdin(body)?;

    let accepted: Accepted = Client::new(api)
        .post(
            "/api/v1/messages",
            &serde_json::json!({ "to": to, "subject": subject, "body": body }),
        )
        .await?;

    // SPEC §8: accepted, not delivered. Saying "sent" would be a promise the
    // outbox has not kept yet.
    println!("{} {}", "queued".green(), accepted.id.dimmed());
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
    attachment_names: Vec<String>,
}

/// List mail (SPEC §10).
pub(crate) async fn inbox(api: &str, unread_only: bool, limit: usize, json: bool) -> Result<()> {
    let path = format!(
        "/api/v1/messages?box=new&limit={limit}{}",
        if unread_only { "&unread=true" } else { "" }
    );
    let summaries: Vec<Summary> = Client::new(api).get(&path).await?;

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
                            "unread": s.unread, "attachment_names": s.attachment_names,
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
            "agent" => "[agent]".magenta().to_string(),
            _ => "[human]".cyan().to_string(),
        };
        let attachments = if summary.attachment_names.is_empty() {
            String::new()
        } else {
            format!(" 📎{}", summary.attachment_names.len())
        };

        println!(
            "{marker} {} {badge} {}{attachments}",
            short_id(&summary.id).dimmed(),
            summary.subject.bold(),
        );
        println!(
            "    {} {}",
            summary.sent_at.format("%Y-%m-%d %H:%M").dimmed(),
            short_node(&summary.from).dimmed()
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    id: String,
    from: String,
    subject: String,
    body: String,
    sender_kind: String,
    sent_at: chrono::DateTime<chrono::Utc>,
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

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": message.id, "from": message.from, "subject": message.subject,
                "body": message.body, "sender_kind": message.sender_kind,
                "sent_at": message.sent_at,
            }))?
        );
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
    Ok(())
}

/// Reply to a message (SPEC §10).
pub(crate) async fn reply(api: &str, id: &str, body: Option<&str>) -> Result<()> {
    let body = body_from_arg_or_stdin(body)?;
    let accepted: Accepted = Client::new(api)
        .post(
            &format!("/api/v1/messages/{id}/reply"),
            &serde_json::json!({ "body": body }),
        )
        .await?;
    println!("{} {}", "queued".green(), accepted.id.dimmed());
    Ok(())
}

/// Rebuild the index from the mail files (SPEC §10).
///
/// This is one of the two commands that touches the store directly, because it
/// exists precisely for when the daemon will not start.
pub(crate) fn reindex(home: Option<&Path>) -> Result<()> {
    let home = paths::home(home)?;
    let store = MailStore::open(home.join("mail")).context("could not open the mail store")?;
    let mut index = Index::open(&home.join("index.db")).context("could not open the index")?;
    index
        .rebuild_from(&store)
        .context("could not rebuild the index")?;

    let counted = index.unread_count()?;
    println!("index rebuilt · {}", unread_phrase(counted));
    Ok(())
}

/// ULIDs are long and the first characters are the timestamp, so the tail is
/// what actually distinguishes two messages sent in the same millisecond.
fn short_id(id: &str) -> String {
    id.chars().skip(id.len().saturating_sub(6)).collect()
}

fn short_node(id: &str) -> String {
    id.strip_prefix("hm1:")
        .and_then(|rest| rest.split('-').next())
        .unwrap_or(id)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_is_phrased_without_a_stray_plural() {
        assert_eq!(unread_phrase(0), "no unread messages");
        assert_eq!(unread_phrase(1), "1 unread message");
        assert_eq!(unread_phrase(7), "7 unread messages");
    }

    #[test]
    fn a_short_id_is_the_distinguishing_tail_not_the_timestamp() {
        assert_eq!(short_id("01JXT21Q00041061050R3GG28A"), "3GG28A");
    }

    #[test]
    fn a_short_id_copes_with_something_shorter_than_it_expects() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn a_node_is_shown_by_its_first_group() {
        assert_eq!(short_node("hm1:w2mq-xor2-seiv"), "w2mq");
        assert_eq!(short_node("not-a-node-id"), "not-a-node-id");
    }

    #[test]
    fn the_openapi_document_builds_without_a_daemon_or_a_home() {
        // `just openapi-check` runs this on a clean tree in CI.
        assert!(ApiDoc::to_json().is_ok());
    }
}
