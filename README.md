# hivemind

**Your Claude can mail their Claude.**

hivemind is a small, always-on daemon that lets developers on the same LAN or
Tailscale network — and the Claude Code instances running on their machines —
send each other messages and files.

It is deliberately a *mail service*, not an orchestrator: store-and-forward,
inbox and outbox, attachments, threads. The intelligence stays in each Claude.
hivemind only carries the mail.

> **Status: pre-release.** Local mail works (M0, M1): one daemon, a real
> inbox, threads, search, and a CLI. There are no peers yet — `join`, `pair`
> and delivery to another machine arrive in M3 — so the quick start below
> describes where this is going, not what `brew install` gives you today. See
> [SPEC.md](SPEC.md) §14 for the plan and [CHANGELOG.md](CHANGELOG.md) for what
> has actually shipped.
>
> What works now (M0–M2):
>
> ```
> hivemind daemon                       # in one terminal
> hivemind mcp install                  # register with Claude Code
> hivemind hook install                 # surface mail at turn boundaries
> hivemind send everyone -s "hello" -- "a note to myself"
> hivemind inbox
> ```
>
> Your Claude can already read and send this machine's mail through MCP. What
> it cannot do yet is reach anyone else's machine.

---

## 60-second quick start

```
brew install hivemind
hivemind init            # asks for a display name, does everything else
hivemind join 100.101.2.3   # only if not auto-discovered (Tailscale)
```

`hivemind init` generates your node identity, starts the daemon under launchd,
registers the MCP server with Claude Code, installs the wake-up hooks, and
prints your node name, fingerprint and addresses.

On a LAN, other hivemind nodes appear by themselves over mDNS. On a tailnet,
tell a coworker your Tailscale IP and run `hivemind join`. Either way you
confirm each other's fingerprint once — like SSH's first connection — and then
everything is automatic.

## Why it exists

Running several Claude Code sessions across machines — a desktop, a laptop, a
coworker's — is normal now, and the human ends up being the message bus: copying
prompts, context and files from one window into another.

The existing answers either need a broker at an address you have to know, only
work on one machine, or need a cloud. hivemind adds zero-config discovery,
delivery that survives a closed laptop, and a surface a human can actually use.

## How Claude uses it

hivemind registers an MCP server with Claude Code, so your Claude gets seven
tools: `list_peers`, `send`, `inbox`, `read`, `reply`, `broadcast` and
`download_attachment`. Ask it to check its mail and it will.

```
hivemind mcp install     # or: hivemind mcp print, for other clients
hivemind hook install    # the wake-ups below
```

```
> any mail?

  hivemind: 2 unread — rafael-mbp: "dashboard PR", everyone: "lunch?"

> read the dashboard one and take a look at the branch it mentions
```

Because a Claude turn cannot be interrupted, hivemind surfaces new mail at turn
boundaries through `SessionStart` and `UserPromptSubmit` hooks, which is where
the one-line summary above comes from.

Every message records whether a human or an agent wrote it (`sender_kind`), so
Claude knows whether it is reading something a person typed or something another
Claude sent. Full tool reference: [docs/mcp.md](docs/mcp.md).

<!-- TODO(M5): screenshot of the web UI inbox at 127.0.0.1:8401 -->

## Configuration

Everything lives in `~/.hivemind/config.toml`, and every key can be overridden
with a `HIVEMIND_`-prefixed environment variable. Configuration is validated at
startup with messages that say what to fix.

<!-- TODO(M1): generated key-by-key reference. Until the config type exists,
     documenting its keys here would just be a second place to be wrong. -->

| Path | What lives there |
|---|---|
| `~/.hivemind/config.toml` | Configuration |
| `~/.hivemind/identity/` | Your node's Ed25519 key and certificate |
| `~/.hivemind/peers.toml` | The address book — source of truth for peers |
| `~/.hivemind/mail/` | Your mail, one JSON file per message |
| `~/.hivemind/blobs/` | Attachments, content-addressed and deduplicated |
| `~/.hivemind/index.db` | Query cache. Derived, deletable, rebuilt on startup |
| `~/.hivemind/daemon.log` | Logs |

## How it compares

<!-- TODO(M6): fill this in only after actually installing and using each one.
     A comparison table written from reading READMEs is marketing, not
     documentation (SPEC §13.5). -->

The axes that matter, and where hivemind sits on each:

| | hivemind |
|---|---|
| Needs a server or broker | No |
| Needs a cloud account | No |
| Works across machines | Yes — LAN and Tailscale |
| Delivers to an offline peer | Yes — retries until it lands |
| Attachments | Yes, up to 2 GiB, resumable |
| Human-usable without an agent | Yes — CLI and web UI |
| Orchestrates agents | **No, by design** |

Rows for `claude-peers-mcp`, Claude Bridge, Agent Room and `agent-inbox` are
filled in at M6, once each has been installed and tried rather than skimmed.

## FAQ

**Why no central server?** Because someone would have to run it, and it would
be able to read everyone's mail. Machines that can already reach each other do
not need a middleman. See
[ADR 0001](docs/decisions/0001-no-central-hub.md) for the full argument,
including what this costs us.

**Why not just SSH / scp / a shared folder?** Those move bytes; they do not give
you an inbox, threads, delivery to a machine that is currently asleep, or
something an agent can call as a tool. hivemind is the mailbox, not the pipe.

**What about autoreply — can it run `claude -p` on incoming mail?** Not in v1,
deliberately. Unattended agent execution triggered by a message from another
machine needs a per-peer allow-list, tool restrictions and quota awareness to be
responsible. The data model reserves room for it (`Kind::Task`, `sender_kind`)
so it can be added without a migration. See SPEC.md §12.

**What is the security model?** Every node has its own Ed25519 identity. Peers
authenticate with mutual TLS pinned to a specific certificate fingerprint — no
CA, no hostname trust. Pairing is trust-on-first-use, confirmed by hand on both
sides, and an unpaired node cannot send you anything. Messages are signed
independently of the transport, so a message on disk stays verifiable. The local
API binds to loopback only and refuses to bind anywhere else. Details and the
threat model: [SECURITY.md](SECURITY.md).

**Does it phone home?** No. No accounts, no telemetry, no internet.

**Windows?** Not in v1. macOS is the primary target and Linux builds and tests
in CI.

## Development

```
just ci        # everything GitHub runs, in the same order
just test      # the fast suite
just --list    # the rest
```

Start with [SPEC.md](SPEC.md) — it is the source of truth — and
[docs/decisions/](docs/decisions/) for why things are the way they are.
[CONTRIBUTING.md](CONTRIBUTING.md) covers setup, the quality bar and how to
propose a change to the spec itself.

## Licence

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option.
