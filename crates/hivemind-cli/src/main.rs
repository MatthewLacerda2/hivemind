//! `hivemind` — the single binary in this workspace.
//!
//! It is both the daemon (`hivemind daemon`, which is what launchd runs) and
//! the client every other subcommand uses to talk to it over
//! `127.0.0.1:8401`. The CLI does not touch `mail/` directly; the two
//! exceptions are `hook check`, which reads the index so it can answer in under
//! 100 ms, and `reindex`, which takes the store lock (SPEC §10).

mod commands;

fn main() {
    // M0 is scaffolding: the command surface lands in M1 (SPEC §14).
    commands::placeholder();
}
