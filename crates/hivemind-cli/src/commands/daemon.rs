//! `hivemind daemon`: the process launchd runs (SPEC §10).
//!
//! The one subcommand that is a server rather than a client, which is why it
//! sits on its own: it opens the store, mounts every router, starts the
//! background tasks and holds the single shutdown signal they all watch.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use hivemind_api::{MailService, NodeDescription};
use hivemind_core::config::Config;
use hivemind_core::identity::Identity;

use crate::paths;

/// Run the daemon in the foreground (SPEC §10).
pub(crate) async fn daemon(home: Option<&Path>, port: u16) -> Result<()> {
    // First, before the store is opened or a directory is created: a
    // reinstall replaces this file while the daemon runs, and the mtime read
    // afterwards would be the new one (#36).
    hivemind_api::freshness::remember();

    let home = paths::home(home)?;
    std::fs::create_dir_all(&home)
        .with_context(|| format!("could not create {}", home.display()))?;

    // SPEC §13.1: JSON to `daemon.log`, pretty in the foreground. Both, not
    // either — somebody watching a terminal wants to read it, and somebody
    // debugging a service started by launchd wants to grep a week of it.
    //
    // The guard has to outlive the daemon: dropping it flushes, and dropping
    // it early means the last lines before a crash are the ones that go
    // missing, which are the ones being looked for.
    let _logging = start_logging(&home);

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
                presence_interval: config.presence_interval,
                tailscale: config.tailscale,
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

/// Start the peer listener, the delivery worker, mDNS, presence and the
/// tailnet sweep.
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
    // complete a TLS handshake, and the router refuses anyone who has not
    // proved the group key.
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

    // SPEC §5.1: advertise on start, browse continuously. Discovery never
    // trusts anybody by itself: a node it finds becomes a peer only by proving
    // the group key when greeted (SPEC §6.2).
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

    // SPEC §5.5: say hello to everybody on start, on wake, and every
    // `presence_interval`. This is what makes a node that comes online
    // visible without anyone running a command, and it is the only thing
    // that carries the peer list between members.
    let presence = tokio::spawn(hivemind_api::presence::say_hello(
        Arc::clone(service),
        std::time::Duration::from_secs(config.presence_interval),
        stop(),
    ));

    Ok(vec![peer_listener, courier, mdns, prefetch, presence])
}

/// Log prettily to the terminal and as JSON to `~/.hivemind/daemon.log`.
///
/// Returns the appender's guard, which must be held for as long as the daemon
/// runs: it flushes on drop, so letting it go early loses exactly the lines
/// written just before whatever went wrong.
///
/// A log file that cannot be opened is not a reason to refuse to run. The
/// terminal layer is set up either way and the failure is said out loud once.
fn start_logging(home: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_env("HIVEMIND_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let pretty = tracing_subscriber::fmt::layer().with_target(false);

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("daemon.log"));

    match file {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            tracing_subscriber::registry()
                .with(filter)
                .with(pretty)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(true)
                        .with_writer(writer),
                )
                .init();
            Some(guard)
        }
        Err(error) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(pretty)
                .init();
            tracing::warn!(
                %error,
                path = %home.join("daemon.log").display(),
                "could not open the log file; logging to the terminal only"
            );
            None
        }
    }
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
