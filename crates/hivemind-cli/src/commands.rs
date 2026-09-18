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
                max_attachment_bytes: config.max_attachment_bytes,
                inline_max_bytes: config.inline_max_bytes,
                prefetch: config.prefetch,
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

    // SPEC §8: `prefetch = true` fetches lazy attachments on arrival rather
    // than on first access. Off by default.
    let prefetch = tokio::spawn(hivemind_api::outbox::prefetch_attachments(
        Arc::clone(service),
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

    Ok(vec![peer_listener, courier, mdns, prefetch])
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
    name: String,
    owner: Option<String>,
    peer_port: u16,
    peers: usize,
    pending_pairs: usize,
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
                "pending_pairs": me.pending_pairs, "outbox": me.outbox,
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

    // These two are what a person is usually looking for when they run this:
    // something is waiting on them, or something is waiting on the network.
    if me.pending_pairs > 0 {
        println!(
            "{} {} — `hivemind peers` to confirm",
            "  pair  ".dimmed(),
            pending_phrase(me.pending_pairs).yellow()
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
        0 => "no peers — `hivemind join <host>` to meet one".to_owned(),
        1 => "1 peer".to_owned(),
        n => format!("{n} peers"),
    }
}

fn pending_phrase(pending: usize) -> String {
    match pending {
        1 => "1 node is waiting for you".to_owned(),
        n => format!("{n} nodes are waiting for you"),
    }
}

fn outbox_phrase(outbox: usize) -> String {
    match outbox {
        1 => "1 message still going out".to_owned(),
        n => format!("{n} messages still going out"),
    }
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
    attach: &[std::path::PathBuf],
    body: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        !to.is_empty(),
        "who is this for? pass a node id, an owner name, or `everyone`"
    );
    let body = body_from_arg_or_stdin(body)?;

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

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": message.id, "from": message.from, "subject": message.subject,
                "body": message.body, "sender_kind": message.sender_kind,
                "sent_at": message.sent_at, "attachments": message.attachments,
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

    if !message.attachments.is_empty() {
        println!();
        println!("{}", "attachments".dimmed());
        for attachment in &message.attachments {
            // "on disk" vs "fetch on read" is the difference between opening
            // it now and waiting for the sender's laptop to be awake.
            let state = if attachment.cached {
                "on disk".green().to_string()
            } else {
                "fetch on read".yellow().to_string()
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

/// What `init` was asked to do (SPEC §2).
#[derive(Debug, Default)]
pub(crate) struct InitOptions {
    pub(crate) name: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) launchd: bool,
    pub(crate) mcp: bool,
    pub(crate) hooks: bool,
}

/// Set this machine up end to end (SPEC §2).
///
/// Every step is skippable and every step is idempotent: running this twice
/// must not generate a second identity, clobber a config somebody edited, or
/// duplicate the hooks. Somebody who ran it, read the output, and ran it again
/// is the normal case, not a misuse.
pub(crate) fn init(home: Option<&Path>, api: &str, options: InitOptions) -> Result<()> {
    let home = paths::home(home)?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("could not create {}", home.display()))?;

    // 1. Identity. `load_or_create` is the idempotence: a second run keeps the
    //    keypair, because changing it would orphan every message already sent.
    let identity = Identity::load_or_create(&home.join("identity"))
        .context("could not create this node's identity")?;

    // 2. Config, with whatever was asked for folded in.
    let config_path = home.join("config.toml");
    let existed = config_path.is_file();
    let mut config = Config::load(&home).context("could not read the configuration")?;
    if let Some(name) = options.name {
        config.name = name;
    }
    if let Some(owner) = options.owner {
        config.owner = Some(owner);
    }
    config.validate()?;
    write_config(&config_path, &config)?;

    println!("{}", "hivemind is set up".green().bold());
    println!();
    println!("  {}   {}", "name".dimmed(), config.name.bold());
    if let Some(owner) = &config.owner {
        println!("  {}  {owner}", "owner".dimmed());
    }
    println!("  {}     {}", "id".dimmed(), identity.node_id());
    println!("  {}  {}", "short".dimmed(), identity.node_id().short());
    println!(
        "  {} {}",
        "config".dimmed(),
        if existed {
            format!("{} (kept your settings)", config_path.display())
        } else {
            config_path.display().to_string()
        }
    );
    println!();

    // 3. launchd.
    if options.launchd {
        crate::service::install(Some(&home))?;
    } else {
        println!(
            "{} launchd skipped — run `hivemind daemon` yourself",
            "--".dimmed()
        );
    }

    // 4. MCP registration. Shelling out to `claude` is what SPEC §2 asks for;
    //    when it is not there, the exact command is printed rather than a
    //    vague suggestion to install something.
    if options.mcp {
        register_mcp(api);
    } else {
        println!("{} MCP registration skipped", "--".dimmed());
    }

    // 5. Hooks, merged rather than clobbered (SPEC §9.3).
    if options.hooks {
        match crate::hooks::install() {
            Ok(()) => {}
            Err(error) => println!("{} could not install the hooks: {error}", "!!".red()),
        }
    } else {
        println!("{} hooks skipped", "--".dimmed());
    }

    // 6. Where other machines can reach this one.
    println!();
    println!("  {}", "reachable at".dimmed());
    for addr in local_addresses(config.peer_port) {
        println!("    {addr}");
    }

    println!();
    println!("Next: `hivemind join <host>` on one machine, then");
    println!("`hivemind pair <short-id>` on both.");
    Ok(())
}

/// Write `config.toml`, preserving anything already in it.
fn write_config(path: &Path, config: &Config) -> Result<()> {
    let text = toml::to_string_pretty(config).context("could not render the configuration")?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, text.as_bytes())
        .with_context(|| format!("could not write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("could not move {} into place", temporary.display()))?;
    Ok(())
}

/// Tell Claude Code about the MCP server (SPEC §2 step 4).
fn register_mcp(api: &str) {
    let command = format!(
        "claude mcp add --scope user --transport http hivemind {}/mcp",
        api.trim_end_matches('/')
    );

    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("claude").is_file()));

    if !on_path {
        println!(
            "{} `claude` is not on PATH. Run this when it is:",
            "--".dimmed()
        );
        println!("    {command}");
        return;
    }

    let result = std::process::Command::new("claude")
        .args([
            "mcp",
            "add",
            "--scope",
            "user",
            "--transport",
            "http",
            "hivemind",
        ])
        .arg(format!("{}/mcp", api.trim_end_matches('/')))
        .output();

    match result {
        Ok(output) if output.status.success() => {
            println!("{} registered with Claude Code", "ok".green());
        }
        // Already registered is the overwhelmingly likely failure, and not one
        // worth alarming anybody about. The command is printed either way.
        Ok(_) | Err(_) => {
            println!("{} could not register automatically. Run:", "--".dimmed());
            println!("    {command}");
        }
    }
}

/// Addresses another machine could reach this one on (SPEC §2 step 6).
fn local_addresses(port: u16) -> Vec<String> {
    let mut addrs = Vec::new();

    // The LAN address, via the routing table rather than by enumerating
    // interfaces: the one that would be used to reach the internet is the one
    // a peer on the same network will see.
    if let Some(ip) = outbound_ip() {
        addrs.push(format!("{ip}:{port}"));
    }

    // Tailscale's, if there is one. Never required (SPEC §5.2).
    if let Ok(output) = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.trim();
            if !line.is_empty() {
                addrs.push(format!("{line}:{port} (tailscale)"));
            }
        }
    }

    if addrs.is_empty() {
        addrs.push(format!("127.0.0.1:{port} (no network found)"));
    }
    addrs
}

/// This machine's address on the network it would use to reach the internet.
///
/// A UDP connect to a public address, which sends nothing — it only asks the
/// routing table which local address would be used. No packets, no DNS, and it
/// works offline as long as there is a route.
fn outbound_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
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
