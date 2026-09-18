# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html),
and the entries below are generated from conventional commits by
[`git-cliff`](https://git-cliff.org).

## [Unreleased]

### Added

- Cargo workspace, the five crates from SPEC §3.1, and the lint policy from
  SPEC §13.1.
- `justfile` defining every check, with `just ci` running exactly what GitHub
  Actions runs.
- CI, nightly and release workflows; Dependabot; pre-commit hooks.
- Architecture decision records 0001–0005, covering the decisions the spec had
  already made: no central hub, files as the source of truth, per-machine
  identity, HTTP for attachments, and `rusqlite` for the index.
- Project documentation: README, CONTRIBUTING, SECURITY with the threat model,
  CODE_OF_CONDUCT, dual Apache-2.0/MIT licensing.

- **M1, local mail.** The message model and its canonical signed encoding, with
  golden vectors derived from an independent reference encoder. The maildir
  store, where files are the source of truth and every write is an atomic
  rename. The SQLite index with full-text search, rebuilt from `mail/` whenever
  it is missing or stale. Node identity: one Ed25519 key that both signs
  messages and backs the self-signed certificate peers will pin. The local HTTP
  API with OpenAPI, Swagger UI, RFC 9457 errors and an SSE event stream. A CLI
  with `daemon`, `status`, `send`, `inbox`, `read`, `reply` and `reindex`.
- Decision records 0006 (node id display), 0007 (`received_at` is not signed)
  and 0008 (the service layer is synchronous).

- **M2, Claude.** The MCP server at `/mcp`, with the seven tools and two
  resources from SPEC §9.1, reachable by Claude Code after
  `hivemind mcp install`. Wake-up hooks (`hivemind hook install`) that print a
  one-line unread summary on `SessionStart` and `UserPromptSubmit` in about
  10 ms, merging into `~/.claude/settings.json` rather than clobbering it.
  Desktop notifications on arriving mail. `config.toml` with `HIVEMIND_*`
  environment overrides and validation. `docs/mcp.md`.

There are no peers yet: `join`, `pair` and delivery to another machine are M3.

[Unreleased]: https://github.com/MatthewLacerda2/hivemind/commits/main
