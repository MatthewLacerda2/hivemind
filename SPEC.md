# hivemind — project specification and kickoff prompt

> **Instructions for Claude Code.** You are creating the `hivemind` repository from scratch. Read this entire document before writing anything. It is the source of truth: when in doubt, follow it; when it is silent, choose the boring, well-tested option and note the decision in `docs/decisions/`. Do not skip milestones or the quality bar to "get something working". This project is meant to be used and maintained by other people.

---

## 1. What this is

**hivemind** is a small, always-on daemon that lets developers on the same LAN or Tailscale network — and the Claude Code instances running on their machines — send each other messages and files. It is deliberately a *mail service*, not an orchestrator: store-and-forward, inbox/outbox, attachments, threads. The intelligence stays in each Claude; hivemind only carries the mail.

Every machine runs one daemon. There is no central server. Peers discover each other automatically on the LAN (mDNS/DNS-SD) or are added by IP/hostname on Tailscale. Mail is delivered peer-to-peer over mutually authenticated TLS and persisted on disk. A local MCP server and a local web UI give Claude and humans the same inbox.

### The one-line pitch
"`brew install hivemind && hivemind init`, then tell a coworker your IP. Now your Claude can mail their Claude."

### Why it exists
Running several Claude Code sessions across machines (desktop, laptop, coworkers) is normal now, and the human is the message bus — copying prompts, context and files between them. Existing tools either need a known broker address, only work on one machine, or need a cloud. hivemind adds zero-config discovery, offline delivery, and a human-usable surface.

### Non-goals (v1)
- No orchestration, task queues, or workflow engine.
- No autoreply / unattended `claude -p` execution. (Design must not preclude it — see §12 — but it does not ship in v1.)
- No relay through the public internet. LAN and Tailscale only. (A relay, when it comes, ships with end-to-end encryption or not at all — ADR 0014.)
- No Windows support in v1. macOS is the primary target; Linux must build and pass tests in CI.
- No accounts, no cloud, no telemetry.

---

## 2. Target users and UX contract

Users are developers who use Claude Code CLI on macOS. They are competent but will not read docs. The entire onboarding must be:

```
brew install MatthewLacerda2/homebrew-tap/hivemind
hivemind init            # asks for a display name, does everything else
hivemind join 100.101.2.3   # only if not auto-discovered (Tailscale)
```

The first line was `brew install hivemind` until it turned out that command
installs [an unrelated process manager](https://github.com/DarthSim/hivemind)
already in Homebrew core. The tap form is a stopgap and is longer than this
section's own standard asks for; see
`docs/decisions/0012-the-name-is-taken.md` for the two ways to get the short
line back. Until a release exists the working install is
`cargo install --path crates/hivemind-cli`.

`hivemind init` must:
1. Generate the node identity (§6).
2. Write `~/.hivemind/config.toml` with sane defaults.
3. Install and start a launchd user agent (`~/Library/LaunchAgents/dev.hivemind.daemon.plist`, `KeepAlive`, `RunAtLoad`).
4. Register the MCP server with Claude Code for the user: `claude mcp add --scope user --transport http hivemind http://127.0.0.1:8401/mcp` (shell out; if `claude` is not on PATH, print the exact command).
5. Install the Claude Code hooks (§9.3) into `~/.claude/settings.json`, merging, never clobbering.
6. Print the node name, fingerprint, and both addresses (LAN IP, Tailscale IP if `tailscale` is on PATH).

Joining is one command with a code somebody pastes you (`hivemind pair <code>`). After that, everything is automatic: every member sees every member, and a machine that comes online is seen by all of them.

---

## 3. Architecture

```
                 ┌───────────────────────── machine A ─────────────────────────┐
                 │  ┌────────────┐   ┌──────────────┐   ┌──────────────────┐   │
   LAN/Tailnet   │  │  Claude    │──▶│  MCP adapter │──▶│                  │   │
  ◀── :8400 ────▶│  └────────────┘   └──────────────┘   │   hivemind       │   │
   mTLS, peers   │  ┌────────────┐   ┌──────────────┐   │   daemon         │   │
                 │  │  Human     │──▶│ web UI / CLI │──▶│  (axum, tokio)   │   │
                 │  └────────────┘   └──────────────┘   └────────┬─────────┘   │
                 │                    127.0.0.1:8401              │             │
                 │                                       ~/.hivemind/mail/     │
                 │                                       (Maildir-style files) │
                 │                                       + index.db (cache)    │
                 └───────────────────────────────────────────────────────────┘
```

One binary, one process, two listeners:

| Listener | Bind | Auth | Purpose |
|---|---|---|---|
| **peer** | `0.0.0.0:8400` | mutual TLS, peer must be paired | daemon ↔ daemon delivery, blob transfer, handshake |
| **local** | `127.0.0.1:8401` | none (loopback only) | HTTP API for humans/CLI, Swagger UI at `/docs`, web UI at `/`, MCP at `/mcp` |

Both listeners serve the **same axum application** with different routers and middleware. The MCP server is a thin adapter over the local HTTP API's handlers (shared service layer, not HTTP-to-HTTP calls). There must be exactly one implementation of every operation.

### 3.1 Crate layout (Cargo workspace)

```
hivemind/
├── Cargo.toml                 # workspace
├── crates/
│   ├── hivemind-core/         # domain types, message model, maildir store, index, peer book, signing
│   ├── hivemind-net/          # discovery (mDNS, tailscale), TLS, peer client, delivery queue
│   ├── hivemind-api/          # axum routers (peer + local), utoipa OpenAPI, web UI assets
│   ├── hivemind-mcp/          # MCP server (rmcp) over the service layer
│   └── hivemind-cli/          # the `hivemind` binary: daemon, init, join, send, inbox, ...
├── web/                       # human UI: single static page, vanilla TS + minimal CSS, built with esbuild, embedded via include_dir
├── docs/
│   ├── decisions/             # ADRs (NNNN-title.md)
│   ├── protocol.md            # wire protocol, generated OpenAPI checked in
│   └── mcp.md                 # tool reference for Claude
├── packaging/
│   ├── homebrew/              # formula
│   └── launchd/               # plist template
├── .github/workflows/
├── justfile
└── README.md
```

Rule: `hivemind-core` has no I/O beyond the filesystem and no async. Everything else depends on it. `hivemind-cli` is the only binary.

---

## 4. Data model

### 4.1 Message

```rust
struct Message {
    id: Ulid,                       // globally unique, time-sortable
    thread_id: Ulid,                // == id for thread roots
    in_reply_to: Option<Ulid>,
    from: NodeId,                   // fingerprint of sender node
    to: Vec<Recipient>,             // NodeId | Owner(String) | Everyone
    subject: String,                // ≤ 200 chars
    body: String,                   // markdown, ≤ 1 MiB
    kind: Kind,                     // Message | Task | Notification
    sender_kind: SenderKind,        // Human | Agent  — set by the entrypoint, never by the caller
    attachments: Vec<AttachmentRef>,
    sent_at: DateTime<Utc>,
    received_at: Option<DateTime<Utc>>,
    signature: Signature,           // Ed25519 over the canonical encoding of the sender-authored
                                    // fields; `received_at` and `signature` itself are excluded
                                    // (see docs/protocol.md and ADR 0007)
}

struct AttachmentRef {
    name: String,
    size: u64,
    sha256: [u8; 32],
    mime: String,
    inline: bool,                   // true → blob shipped with the message; false → fetch on demand
}
```

Canonical encoding for signing: CBOR with deterministic field ordering (`ciborium` + a fixed struct order). Document it in `docs/protocol.md` and cover it with a golden test so the encoding can never silently change.

`sender_kind` is the "was this written by a human or by a Claude" flag. It is set by the local HTTP API based on entrypoint: web UI and CLI → `Human`; MCP → `Agent`. The field is rejected if a caller tries to set it.

### 4.2 Peer

```rust
struct Peer {
    id: NodeId,                     // SHA-256 fingerprint of the node's TLS certificate (Ed25519 SPKI)
    name: String,                   // display name, e.g. "matthew-mbp"
    owner: Option<String>,          // human owner, e.g. "matthew" — for fan-out addressing
    certificate: CertificateDer,    // the whole DER, because §6.3 pins by comparing it
    addrs: Vec<PeerAddr>,           // { host, port, source: Mdns | Tailscale | Manual, last_ok: DateTime }
    paired_at: DateTime<Utc>,
    last_seen: Option<DateTime<Utc>>,
}
```

`certificate` was not in the first draft of this section and is not optional:
`id` is a fingerprint, and a fingerprint cannot be checked against a connection
without the thing it fingerprints. It is also what makes a message verifiable
after the transport is gone (§4.1) — the sender's public key is read out of it
rather than taken from the message, so a peer cannot nominate its own key.

### 4.3 On-disk layout

```
~/.hivemind/
├── config.toml
├── identity/
│   ├── node.key            # Ed25519 private key, 0600
│   └── node.crt            # self-signed X.509, Ed25519 SPKI
├── group.toml              # the group key (§6.2), 0600
├── peers.toml              # the address book: addresses and certificate pins
├── mail/
│   ├── new/<ulid>.json     # unread, received
│   ├── cur/<ulid>.json     # read
│   ├── out/<ulid>.json     # pending delivery (per recipient state inside the file)
│   └── sent/<ulid>.json    # fully delivered
├── blobs/<sha256-hex>      # content-addressed attachments, deduplicated
├── index.db                # SQLite cache — derived, deletable, rebuilt on startup if missing/stale
└── daemon.log
```

**Files are the source of truth.** `index.db` (rusqlite, `bundled`) exists only to answer queries fast (unread count, by thread, by peer, full-text search over subject/body). The daemon must rebuild it from `mail/` on startup if it is missing or its schema version differs, and there must be a `hivemind reindex` command. Writes to `mail/` use write-to-temp + atomic rename. Never write a JSON file in place.

---

## 5. Discovery and addressing

### 5.1 mDNS / DNS-SD (LAN)
- Service type: `_hivemind._tcp.local.`
- Instance name: the node's display name.
- TXT record: `v=1`, `id=<fingerprint>`, `owner=<owner or empty>`, `port=8400`.
- Use the `mdns-sd` crate. Advertise on start; browse continuously; update `peers.toml` addresses (never auto-pair — discovery only tells you a node exists).
- macOS: Bonjour is native; nothing to install. Linux: works alongside avahi.

### 5.2 Tailscale
- If `tailscale` is on PATH, `hivemind peers refresh` runs `tailscale status --json`, extracts peers, and probes `<ip>:8400` with a 1 s timeout to find hivemind nodes. Also try MagicDNS names.
- Never require Tailscale. It is a discovery source, not a dependency.

### 5.3 Manual
- `hivemind join <host-or-ip>[:port]` — contacts the node for when discovery cannot find it. Trust comes from the group key (§6.2), the same as everywhere else; the user is not asked to confirm anything.

### 5.4 Address book policy
- A peer's address list is ordered by `last_ok`. Delivery tries each in order.
- Discovery updates addresses for known peers automatically. A discovered node that does not prove the group key is shown in `hivemind peers` as "seen, not in the group".
- Every hello (§5.5) carries the sender's peer list — id, name, owner, addresses. A member learns the whole group from any one member; addresses learned this way are `source: Gossip` and are tried like any other. Nothing learned by gossip is trusted beyond "try this address": the certificate is what is pinned, and the key is what admits (ADR 0013).

### 5.5 Presence
- `POST /peer/v1/hello` to every known peer on start, on wake, on a change of network, and every `presence_interval` (default 60 s). One request each, no connection held open. A hello carries: proof of the group key (§6.2), the sender's addresses, its peer list (§5.4), and its open sessions (§9.3).
- Receiving a hello marks the sender online, records `last_seen`, takes the `name` and `owner` it reports for itself, wakes the delivery worker for that peer so its queued mail goes now rather than at the next backoff, and forwards "X is up" to the other online peers as a hint on the next hello each of them is sent. A hint is never believed: the receiver sends its own hello to X and marks it online on the answer.
- A hello whose proof does not verify is `403 not_paired` **and the sender is removed from `peers.toml`**, becoming a node merely seen. This is what gives a key rotation (§6.2.4) an effect on a node that is already pinned, and it is acted on from both sides: a peer that answers our own hello with `403` is dropped too. Otherwise whichever node said hello first would retire the other, stop greeting it, and so never give it the evidence to retire back.
- A peer is online while its last hello is younger than two intervals; a delivery failure marks it offline at once. There is no ping. If a minute of staleness ever matters, the interval is a config value, not a design.
- `hivemind peers` and `list_peers` show online, `last_seen` and sessions.

---

## 6. Identity, security, pairing

### 6.1 Identity
One Ed25519 keypair per node, generated by `hivemind init` (`rcgen` for the self-signed cert). The **NodeId is the SHA-256 of the DER-encoded certificate**, displayed as `hm1:<base32-lower, 13 groups of 4>` with a short 8-char prefix for humans. Identity is per *machine*; `owner` is a free-text label so `to: matthew` fans out to all of Matthew's machines.

### 6.2 Group membership
There is one group per node, and being in it is knowing its key (ADR 0013).

1. `hivemind group create` generates 128 random bits, writes them to `group.toml` and prints them as a code. `hivemind pair <code>` writes the same key. That is the whole of setup on every machine.
2. When two nodes first meet — by mDNS, Tailscale, gossip or `join` — each side's `POST /peer/v1/handshake` carries a proof of possession: `HMAC-SHA256(key, cert_sender ‖ cert_receiver ‖ sent_at)`. The exact bytes and a golden vector live in `docs/protocol.md`. Both proofs verify → each side pins the other's certificate in `peers.toml`; nobody is asked anything. A proof that fails is `403 not_paired`. A node already pinned is not a new pairing: its addresses are refreshed and its `name` and `owner` are taken again, because a machine renamed after the first meeting must not go on reading under the name it had then.
3. Every hello (§5.5) carries a fresh proof under the current key. A node is a member while it keeps proving it.
4. Revocation is rotation: `hivemind group create --replace` on any member, and the humans paste the new code on the machines that stay. Without `--replace`, `create` on a node already in a group refuses: an accidental rotation would cut every other machine off until each is given the new code. A node holding the old key fails its next hello and stops receiving mail. There is no per-member revocation in v1.
5. `pair` with a code different from the one held refuses and explains; `--replace` switches. No peer and no message carries a group id — a second group later is a field on `group.toml`, not a migration.

The group is as trustworthy as its least careful member, and `SECURITY.md` says so.

### 6.3 Transport
- rustls, TLS 1.3 only, mutual auth. No CA, no hostnames — pin by fingerprint. **Outbound** connections pin the peer's certificate from `peers.toml`, with one exception: a first contact — `hivemind join`, or a node discovery has only just seen — accepts what the host presents so the group-key proof (§6.2) can run; the certificate is pinned once the proof verifies. **Inbound** connections are admitted by TLS and authorised by the application: `/peer/v1/handshake` is open, everything else requires a paired peer and returns `403 not_paired` otherwise. See `docs/decisions/0010-tls-admits-strangers-the-application-rejects-them.md` — verification happens before the request line is read, so the transport cannot decide this per endpoint.
- Every message is additionally signed (§4.1) so a stored message is verifiable independent of the transport.
- The local listener (8401) binds to `127.0.0.1` only and has no auth. It must refuse to bind to anything else even if configured (fail closed).
- Attachments: size limit configurable (default 2 GiB), path traversal in `name` rejected, blobs stored by hash only.

---

## 7. HTTP API

Use `utoipa` for OpenAPI; serve Swagger UI at `http://127.0.0.1:8401/docs`. **Check the generated `openapi.json` into `docs/`** and add a CI check that fails if it drifts from the code.

### 7.1 Local API (`127.0.0.1:8401`)

```
GET    /api/v1/me                          node identity, addresses, version
GET    /api/v1/peers                       members + seen-not-in-the-group, with online, last_seen, sessions
POST   /api/v1/peers/join      {host}      contact a node discovery cannot find
DELETE /api/v1/peers/{id}
POST   /api/v1/peers/refresh               re-run discovery now
POST   /api/v1/peers/{id}/forget-addr {addr}  drop one address, keeping the peer

GET    /api/v1/group                       joined? created when, member count — never the key
POST   /api/v1/group/create    {replace?}  new key; returns the code once; refuses a node in a group without `replace`
POST   /api/v1/group/join      {code}      store the key; refuses a different one without `replace`
GET    /api/v1/sessions                    open sessions on this node (§9.3)
POST   /api/v1/sessions/{id}   {label}     register or renew one; DELETE removes it

GET    /api/v1/messages?box=new|cur|out|sent&thread=&from=&unread=&q=&limit=&cursor=
       # `cursor` is keyset, not an offset: `<sent_at in milliseconds>:<id>`,
       # built from the last summary already received. An offset would repeat
       # or skip a row when mail arrives mid-pagination, which is the normal
       # state of an inbox.
GET    /api/v1/messages/{id}
POST   /api/v1/messages                    multipart: json part `message` + N file parts → send
       # 202 carries {id, thread_id} and, when this node sent the same thing
       # within two minutes, `duplicate_of` naming it (§8). It is a notice.
POST   /api/v1/messages/{id}/reply         same, with in_reply_to preset
POST   /api/v1/messages/{id}/read          move new → cur
GET    /api/v1/messages/{id}/attachments/{sha}   streams blob; triggers fetch if not inline & not cached
GET    /api/v1/threads/{thread_id}

GET    /api/v1/events                      SSE: message.received, message.delivered, peer.seen, peer.online, peer.offline
GET    /healthz
GET    /docs, /openapi.json
GET    /                                   web UI
POST   /mcp                                MCP streamable HTTP
```

### 7.2 Peer API (`0.0.0.0:8400`, mTLS)

```
POST /peer/v1/handshake        exchange name/owner/version/id + proof of the group key (§6.2)
POST /peer/v1/hello            presence (§5.5): proof, addresses, peer list, sessions; answered in kind
POST /peer/v1/messages         deliver one signed message (+ inline blobs as multipart); idempotent on id
HEAD /peer/v1/blobs/{sha}      does the sender still have it
GET  /peer/v1/blobs/{sha}      range requests supported (resume)
```

Delivery is **push**: the sender's daemon POSTs to each recipient's daemon. Recipients never poll senders for mail — only for lazy blobs.

### 7.3 Errors
RFC 9457 problem+json everywhere. Stable `type` slugs (`not_paired`, `unknown_peer`, `blob_too_large`, ...) enumerated in one Rust enum that generates the docs.

---

## 8. Delivery semantics (store-and-forward)

- `POST /api/v1/messages` writes to `mail/out/` **before** returning `202 Accepted` with the id. Sending never blocks on the network.
- A delivery worker per recipient: try each known address; on success the recipient replies `200` with the received id; mark that recipient delivered. Exponential backoff from 2 s to 5 min, jittered, forever (until the user cancels). A laptop that comes to the office on Monday receives Friday's mail.
- `to: everyone` expands to all paired peers at send time (the expansion is stored, so late joiners don't get it).
- `to: <owner>` expands to all paired peers with that owner.
- Idempotency: recipients dedupe on message id; re-delivery is always safe.
- Idempotency is the **recipient's**, and it cannot cover a sender that sent twice: two presses make two ids, and the far end has no way to tell them from two deliberate messages. So a send whose recipients, subject, body, kind and `in_reply_to` all repeat one this node sent in the last **two minutes** comes back with `duplicate_of` naming it — in the `202`, in the MCP result, and as a warning line from the CLI. A **notice, not a refusal**: the message is queued either way, because asking somebody the same thing again is a real message (#33).
- Inline attachments: any single file ≤ `inline_max` (default 8 MiB) ships in the delivery multipart. Larger ones ship as refs; the recipient fetches lazily on first access or eagerly if `prefetch = true` in config.
- Delivered messages move `out/` → `sent/` only when all recipients are delivered.

---

## 9. Claude integration

### 9.1 MCP server (`hivemind-mcp`, crate `rmcp`, streamable HTTP at `/mcp`)

Tools — keep it to these seven; every one maps 1:1 to a service-layer function:

| Tool | Args | Returns |
|---|---|---|
| `list_peers` | `{}` | peers with name, owner, id, online, last_seen, sessions |
| `send` | `{to: [string], subject, body, kind?, attachments?: [local path]}` | `{id, thread_id, duplicate_of?}` (§8) |
| `inbox` | `{unread_only?: bool, limit?: int, from?: string}` | summaries (id, from, subject, kind, sender_kind, sent_at, attachment names) |
| `read` | `{id}` | full message; marks read; attachment refs include a **local filesystem path** |
| `reply` | `{id, body, attachments?}` | `{id}` |
| `broadcast` | `{subject, body, kind?}` | `{id}` |
| `download_attachment` | `{id, sha}` | `{path}` — local path once fetched |

Resources: `hivemind://inbox` (unread summaries, text) and `hivemind://peers`.

Tool descriptions must tell Claude that `sender_kind: human` means a person typed it directly, and that `to` accepts a node name, an owner name, or `everyone`. Ship `docs/mcp.md` with worked examples.

### 9.2 Registration
`hivemind init` and `hivemind mcp install` register the server with Claude Code (`--scope user`). `hivemind mcp print` prints the JSON snippet for other clients (Claude Desktop, Cursor).

### 9.3 Hooks (wake-up)
A Claude turn cannot be interrupted, so hivemind uses Claude Code hooks to surface mail at turn boundaries:
- `SessionStart`, `UserPromptSubmit` and `SessionEnd` → `hivemind hook check`: prints a one-line summary ("hivemind: 2 unread — rafael-mbp: 'dashboard PR', everyone: 'lunch?'") to stdout if there is unread mail, nothing otherwise. Must exit in < 100 ms. It reads `index.db` directly rather than asking the daemon for the mail, and **never reaches a peer**; the one request it makes is to the loopback API, to register this session (below), with a budget of 80 ms and silence if the daemon is not running. A hook never fails and never waits: an error or a stall interrupts somebody's work to report something they did not ask about.
- `hivemind hook install` / `uninstall` merge into `~/.claude/settings.json` idempotently. Tests cover merging into an existing hooks array without duplicating.
- The same hooks are how the daemon knows which sessions are open: `SessionStart` registers one (labelled by the basename of the working directory), `UserPromptSubmit` renews it, `SessionEnd` removes it. Registering and renewing are one call, because the hook cannot know whether the daemon has heard of the session — one restarted mid-conversation missed the `SessionStart`. A session that stops renewing expires after 30 minutes; nothing is persisted, and only the labels travel to other nodes. The list rides in every hello (§5.5) so other members see `matthew-mbp (online, 2 sessions: hivemind, scorsese)`. A session is presence, never an address (ADR 0003): mail is delivered to the node.

### 9.4 Desktop notification
On `message.received`, post a macOS notification (`osascript`) unless `notifications = false`. Linux: `notify-send` if present. Never fail delivery because notification failed.

---

## 10. CLI

```
hivemind init [--name] [--owner] [--no-launchd] [--no-mcp] [--no-hooks]
hivemind daemon                       # foreground; launchd runs this
hivemind status                       # daemon up? addresses, peer count, unread
hivemind group [create [--replace]]   # show the group; `create` makes a new key and prints the code (rotation is `create --replace`)
hivemind pair <code> [--replace]      # store the key; the one command a new machine runs after `init`
hivemind join <host[:port]>           # contact a node discovery cannot find
hivemind peers [refresh|remove <id>|forget-addr <id> <host:port>]  # online, last seen, sessions
hivemind send <to> -s <subject> [-a file]... [body | -]   # body from arg or stdin
hivemind inbox [--unread] [--box <new|cur|out|sent>] [--json]  # new + cur by default
hivemind sent                         # out + sent: what left here, delivered or not
hivemind read <id>
hivemind reply <id> [body | -]
hivemind reindex
hivemind hook check|install|uninstall
hivemind mcp install|print
hivemind service install|uninstall|restart|logs
hivemind doctor                       # checks: daemon, ports, tailscale, claude on PATH, hooks, mDNS, peer addresses
```

`clap` with derive. `--json` on every read command. Colors via `owo-colors`, respecting `NO_COLOR`. The CLI talks to the daemon over `127.0.0.1:8401`; it never touches `mail/` directly except `hook check` (read-only index) and `reindex` (daemon must be stopped or it takes a lock).

---

## 11. Web UI

One page at `http://127.0.0.1:8401/`, no framework, no build step beyond esbuild for TypeScript, assets embedded in the binary with `include_dir`. Views: inbox (live via SSE), thread, compose (with drag-drop attachments), peers (online, last seen, sessions; join by address). Show `sender_kind` as a small badge ("human" / "agent"). It must be usable without JavaScript for reading (server-rendered list via `askama`); JS enhances it. Accessibility: keyboard navigable, semantic HTML, `prefers-color-scheme`.

---

## 12. Reserved for v2 (do not implement, do not preclude)

- **Autoreply**: daemon spawns `claude -p` on `kind: Task` from whitelisted peers, maps `thread_id` → `--session-id`, mails back the result. Requires per-peer allow-list, `--allowedTools` restriction, and quota awareness. The `Kind::Task` variant and `sender_kind` field exist now so this is additive.
- **Per-member revocation**: a signed revocation that propagates like the peer list. v1 rotates the key instead (§6.2).
- **A second group** on one node. Nothing carries a group id today; when this comes it is a field on `group.toml` and an assignment at index rebuild.
- **Windows** support.
- **Relay** node for peers not on a shared network — shipped together with end-to-end encryption to the recipient, or not at all (ADR 0014).

---

## 13. Quality bar

This section is not optional and is not "later".

### 13.1 Code
- Rust stable, edition 2024, MSRV pinned in `rust-toolchain.toml` and `Cargo.toml`.
- `#![deny(unsafe_code)]` everywhere except an explicitly justified module (there should be none).
- `cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic` with a curated allow-list in `Cargo.toml` `[lints]`. No `#[allow]` without a comment.
- `rustfmt` with a checked-in `rustfmt.toml`.
- Errors: `thiserror` in library crates, `anyhow`/`miette` only in the binary. Every public error type is documented.
- Tracing via `tracing` + `tracing-subscriber` (JSON logs to `daemon.log`, pretty in foreground). Every network operation has a span with peer id and message id.
- All config via `config.toml` + env overrides (`HIVEMIND_*`), documented in README, validated at startup with clear messages.
- No `unwrap()`/`expect()` outside tests except with a `// SAFETY:`-style `// INVARIANT:` comment.
- Public API of `hivemind-core` fully documented (`#![warn(missing_docs)]`).

### 13.2 Tests
- **Unit tests** in every crate. Core: message canonical encoding (golden vectors), signing/verification, maildir atomic moves, index rebuild equivalence (`proptest`: any sequence of mail ops → rebuilt index == live index), address-book ordering, recipient expansion, attachment name validation.
- **Integration tests** (`tests/` in the workspace): spin up two or three real daemons in temp dirs on random ports, pair them, and exercise: send/receive, reply threading, broadcast, inline vs lazy attachments with resume (kill the blob transfer mid-way and assert it resumes with a range request), store-and-forward (stop recipient, send, start recipient, assert delivery), rejection of unpaired senders, idempotent redelivery, index rebuild after deleting `index.db`, hooks merging into an existing `settings.json`.
- **API contract tests**: every endpoint in `openapi.json` has at least one test; a test fails if the checked-in OpenAPI drifts.
- **MCP tests**: use the `rmcp` client to call every tool against a running daemon.
- **mDNS** tests behind `#[ignore]` + a `just test-network` target (multicast is flaky in CI containers); the discovery module must be testable with a trait-mocked backend.
- **Fuzz** (`cargo-fuzz`) targets for: message JSON deserialization, canonical CBOR decode, multipart peer delivery parsing. Run in CI nightly, not on every push.
- Use `cargo-nextest`. Target ≥ 85 % line coverage on `hivemind-core` and `hivemind-net` via `cargo-llvm-cov`; report in CI, fail below threshold.
- Test names describe behavior: `unpaired_peer_delivery_is_rejected_with_403`, not `test_delivery_2`.

### 13.3 Local CI (must mirror GitHub exactly)
- `justfile` with: `fmt`, `lint`, `test`, `test-network`, `cov`, `audit`, `deny`, `openapi-check`, `web-build`, `ci` (runs everything GitHub runs), `release-dry-run`.
- `pre-commit` config (or `lefthook`) running `fmt --check`, `clippy`, `nextest` on staged crates. Document how to install it in `CONTRIBUTING.md`.
- `cargo-deny` config: licenses allow-list (MIT/Apache-2.0/BSD/ISC/MPL-2.0/Unicode), bans on duplicate major versions where practical, advisories fail the build.

### 13.4 GitHub Actions
- `ci.yml` on push and PR: matrix `macos-latest`, `ubuntu-latest`; steps: fmt check, clippy, nextest, doc build with `-D warnings`, openapi drift check, web build, `cargo-deny`, coverage upload (Codecov or artifact + summary). Cache with `Swatinem/rust-cache`. Concurrency group cancels superseded runs.
- `nightly.yml`: fuzz targets (short budget), `cargo-audit`, MSRV check, `cargo update` dry-run report.
- `release.yml`: on tag `v*`, `dist` (formerly `cargo-dist`) builds macOS (arm64 + x86_64) and Linux binaries, generates checksums and a shell installer, creates the GitHub release, and pushes the Homebrew formula to the tap repo. **The file is generated — edit `[workspace.metadata.dist]` and `.github/build-setup.yml`, then run `dist generate`.** On macOS the ad-hoc signature comes from the linker on arm64 (`flags=0x20002(adhoc,linker-signed)`), which is what makes the firewall prompt appear once; x86_64 binaries are not signed, because `dist` has no hook between linking and packaging. See `docs/decisions/0011-cargo-dist-generates-the-release.md`.
- Dependabot for cargo and actions, weekly, grouped.
- Branch protection notes in `CONTRIBUTING.md`: CI must pass, one review, merge commit (**not** squash — see `docs/decisions/0009-merge-commits-not-squash.md`; every commit on a branch must build and pass on its own), conventional commits.

### 13.5 Repo hygiene
- `README.md`: what/why, 60-second quick start, how Claude uses it (with a screenshot placeholder), config reference, FAQ (why no central server, why not SSH, what about autoreply, security model), comparison table with claude-peers-mcp / Claude Bridge / Agent Room / agent-inbox.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` (how to report, what the threat model is), `LICENSE` (Apache-2.0 OR MIT dual), `CHANGELOG.md` (keep-a-changelog, generated by `git-cliff` from conventional commits).
- `docs/decisions/0001-no-central-hub.md`, `0002-files-are-source-of-truth.md`, `0003-identity-is-per-machine.md`, `0004-http-not-sftp-for-files.md`, `0005-rusqlite-over-turso.md` — write these first; they encode the decisions in this spec.
- Issue and PR templates. A `good first issue` label seeded with 5 real issues after M2.

---

## 14. Milestones (each is a mergeable PR set with green CI)

**M0 — Scaffold.** Workspace, crates with empty modules and docs, `justfile`, pre-commit, all GitHub workflows green on an empty project, ADRs 0001–0005, README skeleton, LICENSE, CONTRIBUTING. *Nothing functional yet; CI is the deliverable.*

**M1 — Local mail.** `hivemind-core`: message model, canonical encoding + signing, maildir store, index + rebuild. `hivemind-api` local router: messages/threads/read/SSE, OpenAPI + Swagger, problem+json. CLI: `daemon`, `send` (to self), `inbox`, `read`, `reply`, `reindex`. Integration test: single daemon round-trip. Coverage gate enabled.

**M2 — Claude.** `hivemind-mcp` with all seven tools + resources, `mcp install|print`, hooks `check|install|uninstall`, `docs/mcp.md`. Integration test drives every tool through an `rmcp` client. Desktop notifications.

**M3 — Peers.** Identity generation, rustls mTLS with fingerprint pinning, handshake + TOFU pairing, `peers.toml`, `join`/`pair`/`peers`, peer router, push delivery with backoff, store-and-forward, idempotency. Integration test: two and three daemons. mDNS discovery + Tailscale refresh (mocked backend in tests, real behind `test-network`).

**M4 — Attachments.** Blob store, inline vs lazy, range/resume, size limits, `download_attachment`, dedupe. Integration test with interrupted transfer.

**M5 — Humans.** Web UI (inbox, thread, compose, peers), `sender_kind` badges, `status`, `doctor`.

**M6 — Ship.** `init` end-to-end, launchd `service` commands, `cargo-dist` release pipeline, Homebrew formula, ad-hoc codesign, README complete with comparison table, `CHANGELOG` for `v0.1.0`, tag and release.

**M7 — The group.** ADR 0013 end to end: `group create` / `pair <code>`, proof of the key in the handshake and in every hello, retirement of `pending_pairs` and `--trust-network`; presence (§5.5) with the peer list riding on it; sessions from the hooks (§9.3); Tailscale discovery on by default when a tailnet is there. Integration test: three daemons, one code, every pair paired by nobody; a fourth with the wrong code is `403`; a rotation drops it.

Work milestone by milestone. Open one PR per milestone (or a few per milestone if large), each with a description that links back to the sections of this spec it implements. Do not start M(n+1) until M(n) is green.

---

## 15. Dependencies (pin majors; justify additions in the PR)

`tokio`, `axum`, `tower-http`, `hyper-rustls`/`rustls`, `rcgen`, `ed25519-dalek`, `sha2`, `ciborium`, `serde`/`serde_json`, `ulid`, `chrono`, `rusqlite` (bundled), `mdns-sd`, `reqwest` (rustls, stream), `utoipa` + `utoipa-swagger-ui`, `rmcp`, `clap`, `owo-colors`, `tracing`/`tracing-subscriber`, `thiserror`, `anyhow`, `toml`, `directories`, `include_dir`, `askama`, `proptest`, `tempfile`, `wiremock` (if useful), `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, `cargo-audit`, `cargo-dist`, `git-cliff`. Check each crate's current version and maintenance status before adding it; prefer fewer, better-maintained crates.

---

## 16. First actions for Claude Code

1. Create the repo structure exactly as in §3.1.
2. Write ADRs 0001–0005 from §13.5 using the reasoning in this spec.
3. Write `justfile`, `rust-toolchain.toml`, `deny.toml`, `rustfmt.toml`, `.pre-commit-config.yaml`, and all three workflows. Push, confirm CI is green on the empty workspace.
4. Write the README skeleton with the quick start from §2 verbatim.
5. Begin M1. Start with `hivemind-core` types and the canonical-encoding golden tests, because everything else depends on them.
6. Before each PR: run `just ci` locally; it must pass.

When something in this spec turns out to be wrong or impossible, stop, write an ADR proposing the change, and ask. Do not silently diverge.
