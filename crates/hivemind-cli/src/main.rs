//! `hivemind` — the single binary in this workspace.
//!
//! It is both the daemon (`hivemind daemon`, which is what launchd runs) and
//! the client every other subcommand uses to talk to it over
//! `127.0.0.1:8401`. The CLI does not touch `mail/` directly; the two
//! exceptions are `hook check`, which reads the index so it can answer in under
//! 100 ms, and `reindex`, which takes the store lock (SPEC §10).

mod client;
mod commands;
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
    /// Print the `OpenAPI` document. Used by `just openapi-check`.
    Openapi {
        /// Write to stdout.
        #[arg(long)]
        stdout: bool,
    },
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
    }
}
