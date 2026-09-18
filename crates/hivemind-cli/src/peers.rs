//! `join`, `pair`, `peers` and `pair --trust-network` (SPEC §6.2, §10).
//!
//! Split out of `commands.rs` when it grew past the size gate. They belong
//! together for a better reason than arithmetic: every one of them is about
//! *who this node trusts*, and the confirmation prompt they share is the
//! security boundary — a person comparing a fingerprint by eye is the whole of
//! trust-on-first-use.

use anyhow::{Context as _, Result};
use owo_colors::OwoColorize as _;
use serde::Deserialize;

use crate::client::Client;

/// One peer as the local API reports it.
#[derive(Debug, Deserialize)]
struct PeerRow {
    id: String,
    short_id: String,
    name: String,
    owner: Option<String>,
    addrs: Vec<String>,
    paired: bool,
    last_seen: Option<String>,
}

/// Introduce ourselves to another node (SPEC §6.2, §10).
pub(crate) async fn join(api: &str, host: &str) -> Result<()> {
    let peer: PeerRow = Client::new(api)
        .post("/api/v1/peers/join", &serde_json::json!({ "host": host }))
        .await?;

    println!("{} at {host}", peer.name.bold());
    if let Some(owner) = &peer.owner {
        println!("  owner       {owner}");
    }
    println!("  fingerprint {}", peer.id);

    // The whole point of trust on first use is that a human looks at the
    // fingerprint before anything is trusted, so this stops here deliberately.
    println!();
    println!("Nothing is trusted yet. Compare that fingerprint with the other machine,");
    println!("then run `hivemind pair {}` on both sides.", peer.short_id);
    Ok(())
}

/// Confirm a peer (SPEC §6.2, §10).
pub(crate) async fn pair(api: &str, id: &str, yes: bool) -> Result<()> {
    let client = Client::new(api);
    let peers: Vec<PeerRow> = client.get("/api/v1/peers").await?;
    let peer = peers
        .iter()
        .find(|p| p.short_id.eq_ignore_ascii_case(id) || p.id == id)
        .with_context(|| format!("no peer {id}; run `hivemind peers` to see what is known"))?;

    if peer.paired {
        println!("already paired with {} ({})", peer.name, peer.short_id);
        return Ok(());
    }

    if !yes {
        println!("{}", peer.name.bold());
        if let Some(owner) = &peer.owner {
            println!("  owner       {owner}");
        }
        println!("  fingerprint {}", peer.id);
        println!("  reachable   {}", peer.addrs.join(", "));
        println!();
        if !confirm("Does that fingerprint match what the other machine shows?")? {
            println!("not paired");
            return Ok(());
        }
    }

    let paired: PeerRow = client
        .post(
            &format!("/api/v1/peers/{}/pair", peer.id),
            &serde_json::json!({}),
        )
        .await?;
    println!("paired with {} ({})", paired.name.bold(), paired.short_id);
    println!("Mail flows once the other side has paired too.");
    Ok(())
}

/// List what this node knows (SPEC §10).
pub(crate) async fn list(api: &str, json: bool) -> Result<()> {
    let peers: Vec<PeerRow> = Client::new(api).get("/api/v1/peers").await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&peers_json(&peers))?);
        return Ok(());
    }

    if peers.is_empty() {
        println!("no peers yet — `hivemind join <host>` to meet one");
        return Ok(());
    }

    for peer in &peers {
        let state = if peer.paired {
            "paired".green().to_string()
        } else {
            "pending".yellow().to_string()
        };
        println!("{}  {}  {}", peer.short_id.bold(), state, peer.name);
        if let Some(owner) = &peer.owner {
            println!("    owner     {owner}");
        }
        println!("    addresses {}", peer.addrs.join(", "));
        if let Some(seen) = &peer.last_seen {
            println!("    last seen {seen}");
        }
        if !peer.paired {
            println!("    confirm with `hivemind pair {}`", peer.short_id);
        }
    }
    Ok(())
}

/// Forget a peer (SPEC §10).
pub(crate) async fn remove(api: &str, id: &str) -> Result<()> {
    Client::new(api)
        .delete(&format!("/api/v1/peers/{id}"))
        .await?;
    println!("forgot {id}");
    Ok(())
}

fn peers_json(peers: &[PeerRow]) -> serde_json::Value {
    serde_json::Value::Array(
        peers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "short_id": p.short_id,
                    "name": p.name,
                    "owner": p.owner,
                    "addrs": p.addrs,
                    "paired": p.paired,
                    "last_seen": p.last_seen,
                })
            })
            .collect(),
    )
}

/// Ask a yes/no question on the terminal, defaulting to no.
///
/// Defaulting to no matters: this gates who may send this machine mail, and
/// someone hitting return without reading should not have agreed to anything.
fn confirm(question: &str) -> Result<bool> {
    use std::io::Write as _;

    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("could not read your answer")?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// What `peers refresh` reports.
#[derive(Debug, Deserialize)]
struct Refreshed {
    found: usize,
}

/// Re-run discovery now (SPEC §5.2, §10).
pub(crate) async fn refresh(api: &str) -> Result<()> {
    let result: Refreshed = Client::new(api)
        .post("/api/v1/peers/refresh", &serde_json::json!({}))
        .await?;

    match result.found {
        0 => println!("no new nodes answered"),
        1 => println!("greeted 1 node — `hivemind peers` to see it"),
        n => println!("greeted {n} nodes — `hivemind peers` to see them"),
    }
    Ok(())
}

/// Trust everything discovery found on this LAN (SPEC §6.2.4).
///
/// The loud warning is the point. This is the one command that hands the
/// pairing decision to whoever is on the network, so it says so plainly and
/// asks once, and `--yes` is the only way past it.
pub(crate) async fn trust_network(api: &str, yes: bool) -> Result<()> {
    if !yes {
        eprintln!(
            "{}  every node discovered on this network will be trusted.",
            "WARNING".yellow().bold()
        );
        eprintln!("         Anyone who can reach this LAN can then send this machine mail,");
        eprintln!("         and will be trusted until you run `hivemind peers remove`.");
        eprintln!();
        if !confirm("Do you control every machine on this network?")? {
            println!("nothing trusted");
            return Ok(());
        }
    }

    let paired: Vec<PeerRow> = Client::new(api)
        .post("/api/v1/peers/trust-network", &serde_json::json!({}))
        .await?;

    if paired.is_empty() {
        println!("nothing was waiting — `hivemind peers refresh` to look again");
        return Ok(());
    }
    for peer in &paired {
        println!("paired with {} ({})", peer.name.bold(), peer.short_id);
    }
    Ok(())
}
