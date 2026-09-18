//! Subcommand implementations, one function per verb in SPEC §10.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use hivemind_api::{ApiDoc, MailService, NodeDescription};
use hivemind_core::config::Config;
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

    let config = Config::load(&home).context("could not read the configuration")?;
    let identity = Identity::load_or_create(&home.join("identity"))
        .context("could not load this node's identity")?;
    let private_key = identity
        .private_key_pkcs8()
        .context("could not read this node's private key")?;

    let service = Arc::new(
        MailService::open(
            &home,
            NodeDescription {
                id: identity.node_id(),
                certificate: identity.certificate_der().to_vec(),
                private_key: private_key.clone(),
                name: config.name.clone(),
                owner: config.owner.clone(),
                callback_host: "127.0.0.1".to_owned(),
                peer_port: config.peer_port,
            },
            identity.signing_key().clone(),
        )
        .context("could not open the mail store")?,
    );

    // SPEC §6.3: loopback only, and it fails closed. The address is not
    // configurable, because "bind somewhere else" is not a preference — it is
    // an unauthenticated API on the network.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;

    // The first line is what the integration tests wait for, so it is printed
    // before anything that could be slow.
    println!(
        "hivemind {} listening on http://{addr}",
        identity.node_id().short()
    );
    println!("  docs   http://{addr}/docs");
    println!("  mcp    http://{addr}/mcp");
    println!("  node   {}", identity.node_id());
    tracing::info!(node = %identity.node_id(), %addr, "hivemind is up");

    // One shutdown signal, several listeners. `shutdown()` can only be awaited
    // once, so it is fanned out: a Ctrl-C that stopped the local API but left
    // the peer port open would be worse than no graceful shutdown at all.
    let (stopping, _) = tokio::sync::broadcast::channel::<()>(1);
    tokio::spawn({
        let stopping = stopping.clone();
        async move {
            shutdown().await;
            let _ = stopping.send(());
        }
    });
    let stop = || {
        let mut rx = stopping.subscribe();
        async move {
            let _ = rx.recv().await;
        }
    };

    // Best effort and detached: a notification that fails must never touch
    // delivery (SPEC §9.4).
    tokio::spawn(crate::notify::watch(
        Arc::clone(&service),
        config.notifications,
    ));

    let background = spawn_background(
        &service,
        &config,
        hivemind_net::tls::LocalIdentity::new(identity.certificate_der().to_vec(), private_key),
        &stop,
    )
    .await?;

    // MCP is mounted here rather than inside hivemind-api, so that the API
    // crate does not depend on the MCP crate that depends on it (SPEC §3).
    let router = hivemind_api::router(Arc::clone(&service))
        .nest_service("/mcp", hivemind_mcp::http_service(Arc::clone(&service)));

    axum::serve(listener, router)
        .with_graceful_shutdown(stop())
        .await
        .context("the server stopped unexpectedly")?;

    // All driven by the same signal, so this is a join rather than a wait: it
    // keeps the process alive until a delivery in flight finishes.
    for task in background {
        let _ = task.await;
    }
    Ok(())
}

/// Start the peer listener, the delivery worker and mDNS.
///
/// Returns their handles so the caller can wait for them on the way out.
async fn spawn_background<F, S>(
    service: &Arc<MailService>,
    config: &Config,
    local: hivemind_net::tls::LocalIdentity,
    stop: &F,
) -> Result<Vec<tokio::task::JoinHandle<()>>>
where
    F: Fn() -> S,
    S: std::future::Future<Output = ()> + Send + 'static,
{
    // SPEC §6.3 and ADR 0010: the peer port admits any client that can
    // complete a TLS handshake, and the router refuses anyone unpaired.
    let peer_addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.peer_port));
    let peers = tokio::net::TcpListener::bind(peer_addr)
        .await
        .with_context(|| format!("could not bind {peer_addr}"))?;
    println!("  peers  https://{peer_addr}");

    let peer_listener = tokio::spawn(hivemind_net::listener::serve(
        peers,
        hivemind_net::tls::peer_listener_config(&local)
            .context("could not configure the peer listener")?,
        hivemind_api::peer::router(Arc::clone(service)),
        stop(),
    ));

    // SPEC §8: sending writes to out/ and returns; this is what empties it.
    let courier = tokio::spawn({
        let outbox = hivemind_api::ServiceOutbox::new(Arc::clone(service));
        let transport = hivemind_api::outbox::PeerTransport::new(Arc::clone(service), local);
        let stop = stop();
        async move {
            hivemind_net::delivery::run(&outbox, &transport, chrono::Utc::now, stop).await;
        }
    });

    // SPEC §5.1: advertise on start, browse continuously. Discovery only ever
    // updates addresses — pairing still needs a human on both sides.
    let mdns = tokio::spawn({
        let enabled = config.discovery;
        let sink = hivemind_api::ServiceSink::new(Arc::clone(service));
        let id = service.identity();
        let name = config.name.clone();
        let owner = config.owner.clone();
        let peer_port = config.peer_port;
        let stop = stop();
        async move {
            if !enabled {
                return;
            }
            hivemind_net::discovery::run_mdns(id, &name, owner.as_deref(), peer_port, &sink, stop)
                .await;
        }
    });

    Ok(vec![peer_listener, courier, mdns])
}

/// Wait for whichever comes first: Ctrl-C from a terminal, or SIGTERM.
///
/// launchd stops a service with SIGTERM (SPEC §2), so ignoring it would mean
/// every `hivemind service stop` was really a kill — no graceful shutdown, and
/// no chance to finish a write in progress.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
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

/// Print a one-line unread summary, or nothing (SPEC §9.3).
///
/// This runs on every `SessionStart` and `UserPromptSubmit`, so it has a 100 ms
/// budget. It reads the index directly rather than going through the daemon:
/// no HTTP, no network, and it still works when the daemon is down.
pub(crate) fn hook_check(home: Option<&Path>) {
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
pub(crate) async fn peers(api: &str, json: bool) -> Result<()> {
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
pub(crate) async fn remove_peer(api: &str, id: &str) -> Result<()> {
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
pub(crate) async fn refresh_peers(api: &str) -> Result<()> {
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
