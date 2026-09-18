# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). From v0.2.0 the
entries are generated from conventional commits by
[`git-cliff`](https://git-cliff.org) — run `just changelog`. This first entry is
written by hand, because there is no previous release to diff against and a list
of commit subjects is not a description of what the thing does.

## [0.1.0] - 2026-09-18

The first release. A daemon, a CLI, an MCP server and a web UI, for sending
mail between machines you control.

### Added

**Mail.** Messages with subjects, markdown bodies, threads and full-text search,
stored one JSON file each under `~/.hivemind/mail/`. The files are the source of
truth; the SQLite index beside them is a cache that is rebuilt whenever it is
missing or stale, and `hivemind reindex` rebuilds it on demand.

**Peers.** `hivemind join <host>` introduces two machines; `hivemind pair
<short-id>` on both sides confirms a fingerprint, once, like SSH's first
connection. After that mail flows over TLS 1.3 with mutual authentication,
pinned to the exact certificate each side confirmed. There is no CA, no hostname
trust, and no broker. An unpaired node gets `403`.

**Delivery that survives a closed laptop.** Sending writes to the outbox and
returns; a worker retries with jittered exponential backoff from 2 seconds to 5
minutes, indefinitely. A laptop that comes to the office on Monday receives
Friday's mail. Every message is signed independently of the transport, so one
read off disk a year later still verifies.

**Attachments.** Files up to 2 GiB, content-addressed and deduplicated. Anything
at or below 8 MiB travels with the message; larger files are fetched on first
access, and an interrupted transfer resumes from where it stopped rather than
starting again.

**Discovery.** mDNS on a LAN and `hivemind peers refresh` over Tailscale.
Neither can pair anything — discovery only ever says a node exists.

**Claude Code integration.** An MCP server at `/mcp` with seven tools —
`list_peers`, `send`, `inbox`, `read`, `reply`, `broadcast`,
`download_attachment` — and two resources. Hooks surface unread mail at turn
boundaries. Every message records whether a person or an agent wrote it, and a
caller cannot claim otherwise: it is decided by which entrypoint the request
arrived through.

**A web UI** at `http://127.0.0.1:8401/`: inbox, threads, compose with
drag-and-drop attachments, and pairing with the fingerprints shown. Reading works
with JavaScript switched off.

**Setup.** `hivemind init` generates the identity, writes the config, installs a
launchd agent, registers the MCP server, installs the hooks and prints where the
machine can be reached. Running it twice keeps everything it already made.
`hivemind doctor` checks the parts that break and says what to do about each.

### Known limitations

- No `brew install hivemind` until the tap is published; build from source or
  use the shell installer from the release.
- x86_64 macOS binaries are not ad-hoc codesigned, so an Intel Mac asks about
  the firewall again after an upgrade. Apple Silicon does not.
  See [ADR 0011](docs/decisions/0011-cargo-dist-generates-the-release.md).
- No autoreply: a message cannot trigger `claude -p` on the receiving machine.
  Deliberate — see the FAQ in the README.
- Windows is not supported. Linux builds and is tested in CI; macOS is the
  primary target.

[0.1.0]: https://github.com/MatthewLacerda2/hivemind/releases/tag/v0.1.0
