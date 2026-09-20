//! Subcommand implementations, one function per verb in SPEC §10.
//!
//! The verbs that share a shape live together under `commands/`: the daemon,
//! the mail commands, and `init`. What is left here is what belongs to none of
//! them — the document generator, `status`, and `reindex`, which is one of the
//! two places in the CLI allowed to open the store directly.

use std::path::Path;

use crate::colour::Paint as _;
use anyhow::{Context as _, Result};
use hivemind_api::ApiDoc;
use hivemind_core::index::Index;
use hivemind_core::store::MailStore;
use serde::Deserialize;

use crate::client::Client;
use crate::paths;

mod daemon;
mod init;
mod mail;

pub(crate) use daemon::daemon;
pub(crate) use init::{InitOptions, init};
pub(crate) use mail::{BoxArg, Waited, inbox, read, reply, send, sent, thread, wait};

/// Print the `OpenAPI` document (SPEC §7).
pub(crate) fn openapi() -> Result<()> {
    println!(
        "{}",
        ApiDoc::to_json().context("could not build the document")?
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Me {
    id: String,
    short_id: String,
    version: String,
    unread: u64,
    name: String,
    owner: Option<String>,
    peer_port: u16,
    peers: usize,
    in_group: bool,
    seen: usize,
    outbox: usize,
}

/// Is the daemon up, and what does it know (SPEC §10)?
pub(crate) async fn status(api: &str, json: bool) -> Result<()> {
    let me: Me = Client::new(api).get("/api/v1/me").await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": me.id, "short_id": me.short_id, "version": me.version,
                "unread": me.unread, "name": me.name, "owner": me.owner,
                "peer_port": me.peer_port, "peers": me.peers,
                "in_group": me.in_group, "seen": me.seen, "outbox": me.outbox,
            }))?
        );
        return Ok(());
    }

    println!("{} {}", me.name.bold(), me.short_id.dimmed());
    if let Some(owner) = &me.owner {
        println!("{} {owner}", "  owner ".dimmed());
    }
    println!("{} {}", "  id    ".dimmed(), me.id);
    println!("{} v{}", "  ver   ".dimmed(), me.version);
    println!("{} {}", "  local ".dimmed(), api);
    println!("{} 0.0.0.0:{}", "  peers ".dimmed(), me.peer_port);
    println!();
    println!("{} {}", "  mail  ".dimmed(), unread_phrase(me.unread));
    println!("{} {}", "  known ".dimmed(), peer_phrase(me.peers));

    // These are what a person is usually looking for when they run this:
    // something is stopping mail, or something is waiting on the network.
    if !me.in_group {
        println!(
            "{} {}",
            "  group ".dimmed(),
            "not in one — `hivemind group create`, or `hivemind pair <code>`".yellow()
        );
    }
    if me.seen > 0 {
        println!(
            "{} {} — `hivemind peers` to see them",
            "  seen  ".dimmed(),
            seen_phrase(me.seen).yellow()
        );
    }
    if me.outbox > 0 {
        println!(
            "{} {}",
            "  out   ".dimmed(),
            outbox_phrase(me.outbox).yellow()
        );
    }
    Ok(())
}

fn peer_phrase(peers: usize) -> String {
    match peers {
        0 => "no peers yet — machines in the same group find each other on the LAN".to_owned(),
        1 => "1 peer".to_owned(),
        n => format!("{n} peers"),
    }
}

fn seen_phrase(seen: usize) -> String {
    match seen {
        1 => "1 node answered that is not in this group".to_owned(),
        n => format!("{n} nodes answered that are not in this group"),
    }
}

fn outbox_phrase(outbox: usize) -> String {
    match outbox {
        1 => "1 message still going out".to_owned(),
        n => format!("{n} messages still going out"),
    }
}

pub(crate) fn unread_phrase(unread: u64) -> String {
    match unread {
        0 => "no unread messages".to_owned(),
        1 => "1 unread message".to_owned(),
        n => format!("{n} unread messages"),
    }
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

pub(crate) fn short_node(id: &str) -> String {
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
