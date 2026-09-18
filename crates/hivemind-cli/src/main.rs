//! `hivemind` — the single binary in this workspace.
//!
//! It is both the daemon (`hivemind daemon`, which is what launchd runs) and
//! the client every other subcommand uses to talk to it over
//! `127.0.0.1:8401`. The CLI does not touch `mail/` directly; the two
//! exceptions are `hook check`, which reads the index so it can answer in under
//! 100 ms, and `reindex`, which takes the store lock (SPEC §10).

mod client;
mod commands;
mod hooks;
mod notify;
mod paths;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Mail for developers and their Claude Code instances.
#[derive(Debug, Parser)]
#[command(name = "hivemind", version, about, long_about = None)]
struct Cli {
    /// Where hivemind keeps its data.
    #[arg(long, env = "HIVEMIND_HOME", global = true)]
    home: Option<std::path::PathBuf>,

    /// The local API to talk to.
    #[arg(
        long,
        env = "HIVEMIND_API",
        global = true,
        default_value = "http://127.0.0.1:8401"
    )]
    api: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground. This is what launchd runs.
    Daemon {
        /// The loopback port to serve on.
        #[arg(long, default_value_t = 8401)]
        port: u16,
    },
    /// Is the daemon up, and what does it know?
    Status {
        /// Print JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Send a message.
    Send {
        /// A node id, an owner name, or `everyone`.
        to: Vec<String>,
        /// The subject.
        #[arg(short, long)]
        subject: String,
        /// The body. Omit, or pass `-`, to read it from stdin.
        #[arg(last = true)]
        body: Option<String>,
    },
    /// List your mail.
    Inbox {
        /// Only unread messages.
        #[arg(long)]
        unread: bool,
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Read one message, and mark it read.
    Read {
        /// The message id.
        id: String,
        /// Print JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
    /// Reply to a message.
    Reply {
        /// The message being replied to.
        id: String,
        /// The body. Omit, or pass `-`, to read it from stdin.
        body: Option<String>,
    },
    /// Rebuild the query index from the mail files.
    Reindex,
    /// Surface unread mail to Claude at turn boundaries.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Register the MCP server with Claude Code and other clients.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Print the `OpenAPI` document. Used by `just openapi-check`.
    Openapi {
        /// Write to stdout.
        #[arg(long)]
        stdout: bool,
    },
    /// Introduce yourself to another node (SPEC §6.2).
    ///
    /// This is the first half of trust on first use: it records the other
    /// node's fingerprint but trusts nothing. Both sides then run `pair`.
    Join {
        /// A hostname or address, with an optional `:port`.
        host: String,
    },
    /// Confirm a peer, after checking its fingerprint (SPEC §6.2).
    Pair {
        /// The peer's short id, or its full `hm1:` form.
        ///
        /// Omit it only with --trust-network.
        id: Option<String>,
        /// Skip the confirmation prompt. For scripts, and for tests.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Trust every node found on this LAN, without looking at any of them.
        ///
        /// For a network you fully control. Anyone who can reach it can then
        /// send this machine mail.
        #[arg(long, conflicts_with = "id")]
        trust_network: bool,
    },
    /// Who this node knows.
    Peers {
        #[command(subcommand)]
        action: Option<PeerCommand>,
        /// Print JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    /// Look for peers again now, rather than waiting for discovery.
    Refresh,
    /// Forget a peer. Mail from it is refused from that moment on.
    Remove {
        /// The peer's short id, or its full `hm1:` form.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Print a one-line summary if there is unread mail, nothing otherwise.
    ///
    /// Runs on every Claude turn boundary, so it reads the index directly and
    /// never touches the network (SPEC §9.3).
    Check,
    /// Add the hooks to ~/.claude/settings.json, merging rather than clobbering.
    Install,
    /// Take the hooks back out, leaving everything else alone.
    Uninstall,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Register with Claude Code for this user.
    Install,
    /// Print the JSON snippet other MCP clients need.
    Print,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // `openapi` is pure code generation: it must work with no daemon, no
        // data directory and no network, because CI runs it on a clean tree.
        Command::Openapi { .. } => commands::openapi(),
        Command::Daemon { port } => commands::daemon(cli.home.as_deref(), port).await,
        Command::Status { json } => commands::status(&cli.api, json).await,
        Command::Send { to, subject, body } => {
            commands::send(&cli.api, &to, &subject, body.as_deref()).await
        }
        Command::Inbox {
            unread,
            limit,
            json,
        } => commands::inbox(&cli.api, unread, limit, json).await,
        Command::Read { id, json } => commands::read(&cli.api, &id, json).await,
        Command::Reply { id, body } => commands::reply(&cli.api, &id, body.as_deref()).await,
        Command::Reindex => commands::reindex(cli.home.as_deref()),
        Command::Join { host } => commands::join(&cli.api, &host).await,
        Command::Pair {
            id,
            yes,
            trust_network,
        } => match (trust_network, id) {
            (true, _) => commands::trust_network(&cli.api, yes).await,
            (false, Some(id)) => commands::pair(&cli.api, &id, yes).await,
            (false, None) => {
                anyhow::bail!("which peer? give a short id, or --trust-network to take them all")
            }
        },
        Command::Peers { action, json } => match action {
            None => commands::peers(&cli.api, json).await,
            Some(PeerCommand::Refresh) => commands::refresh_peers(&cli.api).await,
            Some(PeerCommand::Remove { id }) => commands::remove_peer(&cli.api, &id).await,
        },
        Command::Hook(HookCommand::Check) => {
            commands::hook_check(cli.home.as_deref());
            Ok(())
        }
        Command::Hook(HookCommand::Install) => hooks::install(),
        Command::Hook(HookCommand::Uninstall) => hooks::uninstall(),
        Command::Mcp(McpCommand::Install) => hooks::mcp_install(&cli.api),
        Command::Mcp(McpCommand::Print) => hooks::mcp_print(&cli.api),
    }
}
