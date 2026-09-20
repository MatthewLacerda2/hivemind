# hivemind

**Your Claude can mail their Claude.**

hivemind is a small, always-on daemon that lets developers on the same LAN or
Tailscale network — and the Claude Code instances running on their machines —
send each other messages and files.

It is deliberately a *mail service*, not an orchestrator: store-and-forward,
inbox and outbox, attachments, threads. The intelligence stays in each Claude.
hivemind only carries the mail.

> **Status: pre-release.** Everything below works: local mail, pairing over
> mutual TLS, delivery that survives a closed laptop, attachments, the MCP
> server and the web UI (M0–M5). What is not done is distribution — there is no
> `brew install hivemind` yet, so today it is `cargo install --path
> crates/hivemind-cli` or a build from the repo. See
> [CHANGELOG.md](CHANGELOG.md) for what has shipped and
> [SPEC.md](SPEC.md) §14 for the plan.

---

## 60-second quick start

```
git clone https://github.com/MatthewLacerda2/hivemind && cd hivemind
cargo install --path crates/hivemind-cli --locked
hivemind init
```

That is it. `hivemind init` generates your node identity, starts the daemon
under launchd, registers the MCP server with Claude Code, installs the wake-up
hooks, and prints your node name, fingerprint and addresses.

> **Never `brew install hivemind`.** That name belongs to
> [a different program](https://github.com/DarthSim/hivemind) — a process
> manager — already in Homebrew core. Running it installs the wrong software
> and the next line then fails confusingly.
>
> Once there is a release it becomes
> `brew install MatthewLacerda2/homebrew-tap/hivemind`, tap included. The short
> form stays wrong; [ADR 0012](docs/decisions/0012-the-name-is-taken.md) has
> the two ways to get it back.

One machine runs `hivemind group create` and prints a code; every other machine
runs `hivemind pair <code>`. That is the whole of setup. Machines in the same
group find each other on a LAN by themselves; on a tailnet, `hivemind join
<their-tailscale-ip>` once. Nobody confirms anything by hand, and every member
reaches every other.

## Why it exists

Running several Claude Code sessions across machines — a desktop, a laptop, a
coworker's — is normal now, and the human ends up being the message bus: copying
prompts, context and files from one window into another.

The existing answers either need a broker at an address you have to know, only
work on one machine, or need a cloud. hivemind adds zero-config discovery,
delivery that survives a closed laptop, and a surface a human can actually use.

## How Claude uses it

hivemind registers an MCP server with Claude Code, so your Claude gets nine
tools: `list_peers`, `send`, `inbox`, `chats`, `read`, `thread`, `reply`,
`broadcast` and `download_attachment`. Ask it to check its mail and it will.

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

The same mail is at `http://127.0.0.1:8401/` — inbox, threads, compose with
drag-and-drop attachments, and who is in the group and who was only seen.
It reads without JavaScript.

## Configuration

Everything lives in `~/.hivemind/config.toml`, and every key can be overridden
with a `HIVEMIND_`-prefixed environment variable. Configuration is validated at
startup with messages that say what to fix.

| Key | Default | What it does |
|---|---|---|
| `name` | this machine's hostname | What peers show for this node |
| `owner` | unset | Who owns it, so `hivemind send <owner>` reaches every machine they run |
| `peer_port` | `8400` | Where other nodes connect. Bound on `0.0.0.0` |
| `local_port` | `8401` | The loopback API and web UI. Never bound anywhere else |
| `notifications` | `true` | A desktop notification when mail arrives |
| `discovery` | `true` | Advertise and browse over mDNS. Off for a network you would rather not announce yourself on |
| `prefetch` | `false` | Fetch large attachments on arrival rather than on first read |
| `max_attachment_bytes` | `2 GiB` | The largest attachment this node accepts |
| `inline_max_bytes` | `8 MiB` | At or below this, a file travels with its message |
| `presence_interval` | `60` | Seconds between saying hello to every peer. `0` turns presence off |
| `tailscale` | `auto` | Find peers through Tailscale. `auto` uses it if it is there; `true` expects it and `doctor` says so if it is not; `false` never touches it |

A test in `hivemind-core` reads this table and fails if a `Config` field is
missing from it — the alternative is a second place to be wrong.


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

Read from each project's own documentation on 18 September 2026, not from
running them. Where a project does not say, the cell says so rather than
guessing — an empty cell is more honest than an assumption, and these move
fast, so check before relying on any row.

| | hivemind | [claude-peers-mcp][cpm] | [claude-bridge][cb] | [agent-inbox][ai] |
|---|---|---|---|---|
| Needs a broker or server | No | Yes — a daemon on `localhost:7899` | No | Yes — a LangGraph deployment |
| Needs a cloud account | No | No | No | Optional — hosted or self-host |
| Works across machines | Yes — LAN and Tailscale | No — "everything is localhost-only" | No — "runs locally, on one machine" | n/a |
| Reaches a peer that is asleep | Yes — retries until it lands | No — live sessions only | Yes — waits in the inbox | n/a |
| Attachments | Yes, up to 2 GiB, resumable | Not documented | Not documented | Not documented |
| Usable by a human with no agent | Yes — CLI and web UI | CLI | Through a Claude Code chat | It is a human UI |
| Orchestrates agents | **No, by design** | No | No — coordinates chats you opened | n/a — reviews interrupts |

[cpm]: https://github.com/louislva/claude-peers-mcp
[cb]: https://github.com/michalekz/claude-bridge
[ai]: https://github.com/langchain-ai/agent-inbox

`agent-inbox` is in the table because SPEC §13.5 names it, but it is answering a
different question: it is a human-in-the-loop UI for reviewing a LangGraph
agent's interrupts, not a way for two people's agents to talk. The `n/a` rows
are not gaps — they are the wrong question to ask of it.

SPEC §13.5 also names "Agent Room". Several unrelated projects use that name and
none is clearly the one meant, so there is no row for it rather than a row about
whichever one came up first.

**Where hivemind is the wrong tool.** If every session is on one machine,
`claude-bridge` is simpler and has no network surface at all. If what you want
is a human reviewing an agent's decisions, that is `agent-inbox`. hivemind earns
its complexity — mutual TLS, a pairing step, a retry queue — only when the
machines are genuinely different machines.

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
CA, no hostname trust. Membership is knowing the group's key — 128 random bits,
proved in the handshake and never sent — and a node outside the group cannot
send you anything. The group is as trustworthy as its least careful member, so
the code is shared like a password. Messages are signed
independently of the transport, so a message on disk stays verifiable. The local
API binds to loopback only and refuses to bind anywhere else. Details and the
threat model: [SECURITY.md](SECURITY.md).

**Does it phone home?** No. No accounts, no telemetry, no internet.

**Windows?** Not in v1. macOS is the primary target and Linux builds and tests
in CI.

## Being handed this and told to run it

[`docs/using.md`](docs/using.md) is the whole of it — install, `hivemind init`,
and `hivemind pair` with the code whoever sent you has.

An agent asked to do it has a skill for exactly that,
`.claude/skills/run-hivemind/`, which it will find on its own. `CLAUDE.md` is
for working *on* hivemind and says so in its first line.

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
