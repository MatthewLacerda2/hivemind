//! `hivemind` — the single binary in this workspace.
//!
//! It is both the daemon (`hivemind daemon`, which is what launchd runs) and
//! the client every other subcommand uses to talk to it over
//! `127.0.0.1:8401`. The CLI does not touch `mail/` directly; the two
//! exceptions are `hook check`, which reads the index so it can answer in under
//! 100 ms, and `reindex`, which takes the store lock (SPEC §10).

mod client;
mod colour;
mod commands;
mod doctor;
mod hooks;
mod notify;
mod paths;
mod peers;
mod service;

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
        /// A file to send with it. Repeat for several.
        ///
        /// Copied into hivemind's own storage straight away, so the original
        /// can be moved or deleted afterwards.
        #[arg(short = 'a', long = "attach")]
        attach: Vec<std::path::PathBuf>,
        /// The body. Omit, or pass `-`, to read it from stdin.
        #[arg(last = true)]
        body: Option<String>,
    },
    /// List your mail.
    Inbox {
        /// One box only. Omit for everything that arrived, read or not.
        #[arg(long = "box", value_enum)]
        r#box: Option<commands::BoxArg>,
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
    /// List what you sent, including what is still on its way.
    Sent {
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
    /// Show the group this machine is in, or make one (SPEC §6.2).
    Group {
        #[command(subcommand)]
        action: Option<GroupCommand>,
    },
    /// Join a group with the code another member printed (SPEC §6.2).
    ///
    /// The one command a new machine runs after `init`. Every machine in the
    /// group it can reach becomes a peer, with nobody asked anything: the code
    /// is the decision.
    Pair {
        /// The group's code, `hm-…`, as `hivemind group create` printed it.
        code: String,
        /// Leave the group this machine is already in, for this one.
        #[arg(long)]
        replace: bool,
    },
    /// Contact a machine discovery cannot find (SPEC §5.3).
    ///
    /// Becomes a peer if it is in this machine's group; otherwise it is listed
    /// as seen. Nothing is confirmed by hand.
    Join {
        /// A hostname or address, with an optional `:port`.
        host: String,
    },
    /// Set this machine up end to end (SPEC §2).
    Init {
        /// The name peers see. Defaults to this machine's hostname.
        #[arg(long)]
        name: Option<String>,
        /// The person who owns this machine, for `to: <owner>` addressing.
        #[arg(long)]
        owner: Option<String>,
        /// Skip the launchd agent.
        #[arg(long)]
        no_launchd: bool,
        /// Skip registering the MCP server with Claude Code.
        #[arg(long)]
        no_mcp: bool,
        /// Skip installing the Claude Code hooks.
        #[arg(long)]
        no_hooks: bool,
    },
    /// Run hivemind in the background, at login (SPEC §10).
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Check that everything hivemind needs is working (SPEC §10).
    Doctor {
        /// Print JSON instead of prose.
        #[arg(long)]
        json: bool,
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
enum GroupCommand {
    /// Make a new group and print its code.
    Create {
        /// Replace the group this machine is in. This is how the key rotates:
        /// the old code stops working, and every machine that should stay
        /// needs the new one.
        #[arg(long)]
        replace: bool,
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
    /// Forget one of a peer's addresses, keeping the peer.
    ///
    /// For an address that cannot work — `hivemind doctor` names one that
    /// points at this machine — where forgetting the peer would cost the whole
    /// trust relationship over one line.
    ForgetAddr {
        /// The peer's short id, or its full `hm1:` form.
        id: String,
        /// The address, as `hivemind peers` prints it: `host:port`.
        addr: String,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Print a one-line summary if there is unread mail, nothing otherwise,
    /// and register this session with the daemon.
    ///
    /// Runs on every Claude turn boundary, so it reads the index directly
    /// rather than asking the daemon for the mail, and speaks to the daemon
    /// only over loopback and only briefly (SPEC §9.3). It never reaches a
    /// peer, and it never fails: a hook that errors interrupts somebody's
    /// work to report something they did not ask about.
    Check,
    /// Add the hooks to ~/.claude/settings.json, merging rather than clobbering.
    Install,
    /// Take the hooks back out, leaving everything else alone.
    Uninstall,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Write the launchd agent and start it.
    Install,
    /// Stop the agent and remove it.
    Uninstall,
    /// Stop and start it, picking up a new binary or config.
    Restart,
    /// Show what the daemon has been saying.
    Logs {
        /// How many lines to show.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Keep printing as more arrive.
        #[arg(short, long)]
        follow: bool,
    },
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

    // Before anything prints. SPEC §10 promises `NO_COLOR` is respected, and
    // output that is being read by a script or another Claude must not carry
    // escape codes it will try to parse (#55).
    colour::init();

    match cli.command {
        // `openapi` is pure code generation: it must work with no daemon, no
        // data directory and no network, because CI runs it on a clean tree.
        Command::Openapi { .. } => commands::openapi(),
        Command::Daemon { port } => commands::daemon(cli.home.as_deref(), port).await,
        Command::Status { json } => commands::status(&cli.api, json).await,
        Command::Send {
            to,
            subject,
            attach,
            body,
        } => commands::send(&cli.api, &to, &subject, &attach, body.as_deref()).await,
        Command::Inbox {
            r#box,
            unread,
            limit,
            json,
        } => commands::inbox(&cli.api, r#box, unread, limit, json).await,
        Command::Sent { limit, json } => commands::sent(&cli.api, limit, json).await,
        Command::Read { id, json } => commands::read(&cli.api, &id, json).await,
        Command::Reply { id, body } => commands::reply(&cli.api, &id, body.as_deref()).await,
        Command::Reindex => commands::reindex(cli.home.as_deref()),
        Command::Init {
            name,
            owner,
            no_launchd,
            no_mcp,
            no_hooks,
        } => commands::init(
            cli.home.as_deref(),
            &cli.api,
            commands::InitOptions {
                name,
                owner,
                launchd: !no_launchd,
                mcp: !no_mcp,
                hooks: !no_hooks,
            },
        ),
        Command::Service { action } => match action {
            ServiceCommand::Install => service::install(cli.home.as_deref()),
            ServiceCommand::Uninstall => service::uninstall(),
            ServiceCommand::Restart => service::restart(),
            ServiceCommand::Logs { lines, follow } => {
                service::logs(cli.home.as_deref(), lines, follow)
            }
        },
        Command::Doctor { json } => doctor::run(cli.home.as_deref(), &cli.api, json).await,
        Command::Join { host } => peers::join(&cli.api, &host).await,
        Command::Group { action } => match action {
            None => peers::group(&cli.api).await,
            Some(GroupCommand::Create { replace }) => peers::create(&cli.api, replace).await,
        },
        Command::Pair { code, replace } => peers::pair(&cli.api, &code, replace).await,
        Command::Peers { action, json } => match action {
            None => peers::list(&cli.api, json).await,
            Some(PeerCommand::Refresh) => peers::refresh(&cli.api).await,
            Some(PeerCommand::Remove { id }) => peers::remove(&cli.api, &id).await,
            Some(PeerCommand::ForgetAddr { id, addr }) => {
                peers::forget_addr(&cli.api, &id, &addr).await
            }
        },
        Command::Hook(HookCommand::Check) => {
            hooks::check::hook_check(cli.home.as_deref(), &cli.api).await;
            Ok(())
        }
        Command::Hook(HookCommand::Install) => hooks::install(),
        Command::Hook(HookCommand::Uninstall) => hooks::uninstall(),
        Command::Mcp(McpCommand::Install) => hooks::mcp_install(&cli.api),
        Command::Mcp(McpCommand::Print) => hooks::mcp_print(&cli.api),
    }
}
