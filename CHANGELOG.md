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

Nothing is functional yet — M0's deliverable is green CI on an empty workspace
(SPEC §14).

[Unreleased]: https://github.com/MatthewLacerda2/hivemind/commits/main
