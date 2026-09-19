//! `group`, `pair`, `join` and `peers` (SPEC §6.2, §10).
//!
//! Split out of `commands.rs` when it grew past the size gate. They belong
//! together for a better reason than arithmetic: every one of them is about
//! *who this node trusts*. Since ADR 0013 that is decided by one secret — the
//! group key — rather than a prompt per machine, so none of these asks a
//! question; the code a person pastes is the whole of the decision.

use anyhow::Result;
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

/// Which group this node is in, as the local API reports it.
#[derive(Debug, Deserialize)]
struct GroupRow {
    in_group: bool,
    joined_at: Option<String>,
    members: usize,
}

/// A group that was just made.
#[derive(Debug, Deserialize)]
struct Created {
    code: String,
}

/// Show the group (SPEC §10).
pub(crate) async fn group(api: &str) -> Result<()> {
    let group: GroupRow = Client::new(api).get("/api/v1/group").await?;
    if !group.in_group {
        println!("not in a group");
        println!();
        println!("On the first machine: `hivemind group create`.");
        println!("On every other: `hivemind pair <code>`, with the code it printed.");
        return Ok(());
    }

    println!("in a group since {}", group.joined_at.unwrap_or_default());
    println!("  {}", member_phrase(group.members));
    Ok(())
}

/// Make a new group, or rotate the key of the one this node is in (SPEC §6.2).
pub(crate) async fn create(api: &str, replace: bool) -> Result<()> {
    let created: Created = Client::new(api)
        .post(
            "/api/v1/group/create",
            &serde_json::json!({ "replace": replace }),
        )
        .await?;

    // Plain, never styled: it is there to be copied, and a colour code picked
    // up with it is a code that no longer parses.
    println!("{}", created.code);
    println!();
    println!("That is the group's code. On every other machine, run");
    println!("  hivemind pair {}", created.code);
    println!("Anyone who has it can join, so share it the way you would a password.");
    if replace {
        println!();
        println!(
            "{} the old code no longer works. Machines that should stay need this one.",
            "Rotated:".yellow()
        );
    }
    Ok(())
}

/// Join the group a code belongs to (SPEC §6.2, §10).
pub(crate) async fn pair(api: &str, code: &str, replace: bool) -> Result<()> {
    let group: GroupRow = Client::new(api)
        .post(
            "/api/v1/group/join",
            &serde_json::json!({ "code": code, "replace": replace }),
        )
        .await?;

    println!("in the group");
    // Admission happens as other members are met, which is under way in the
    // background; zero here is the ordinary answer a second after pairing.
    println!("  {}", member_phrase(group.members));
    println!();
    println!("Machines in the group on this network are being greeted now;");
    println!("`hivemind peers` in a moment shows who answered.");
    Ok(())
}

/// Contact a node discovery cannot find (SPEC §5.3, §10).
pub(crate) async fn join(api: &str, host: &str) -> Result<()> {
    let peer: PeerRow = Client::new(api)
        .post("/api/v1/peers/join", &serde_json::json!({ "host": host }))
        .await?;

    if peer.paired {
        println!("{} ({}) is in the group", peer.name.bold(), peer.short_id);
    } else {
        println!(
            "{} ({}) answered, but is not in this node's group",
            peer.name.bold(),
            peer.short_id
        );
        println!("If it should be, give that machine the code: `hivemind pair <code>`.");
    }
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
        println!("no peers yet — machines in the same group find each other on the LAN");
        println!("`hivemind join <host>` reaches one discovery cannot see");
        return Ok(());
    }

    for peer in &peers {
        let state = if peer.paired {
            "in the group".green().to_string()
        } else {
            "seen, not in the group".yellow().to_string()
        };
        println!("{}  {}  {}", peer.short_id.bold(), state, peer.name);
        if let Some(owner) = &peer.owner {
            println!("    owner     {owner}");
        }
        println!("    addresses {}", peer.addrs.join(", "));
        if let Some(seen) = &peer.last_seen {
            println!("    last seen {seen}");
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

fn member_phrase(members: usize) -> String {
    match members {
        0 => "no other members met yet".to_owned(),
        1 => "1 other member met".to_owned(),
        n => format!("{n} other members met"),
    }
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
        0 => println!("no nodes answered"),
        1 => println!("1 node answered — `hivemind peers` to see it"),
        n => println!("{n} nodes answered — `hivemind peers` to see them"),
    }
    Ok(())
}
