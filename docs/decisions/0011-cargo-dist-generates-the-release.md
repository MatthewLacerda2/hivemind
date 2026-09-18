# 11. `dist` generates the release, and we lose one thing by it

Date: 2026-09-18

## Status

Accepted. Amends SPEC §13.4.

## Context

SPEC §13.4 asks for two things from the release pipeline that pull in opposite
directions:

> `cargo-dist` builds macOS (arm64 + x86_64) and Linux binaries, generates
> checksums, creates the GitHub release, and updates the Homebrew formula in
> `packaging/homebrew` […] Binaries ad-hoc codesigned on macOS (`codesign -s -`)
> so the firewall prompt appears once.

M0 shipped a handwritten `release.yml` — a build matrix, a `codesign -s -` step,
a `tar`, a `shasum` and `gh release create` — with a note saying M6 would
evaluate replacing it with `cargo-dist`. This is that evaluation.

`cargo-dist` is now published as `dist` (crate `cargo-dist`, binary `dist`),
version 0.32.0, actively maintained.

## What `dist` does that the handwritten workflow does not

- **Generates the Homebrew formula**, with the right per-architecture URLs and
  checksums, and pushes it to a tap. Doing this by hand means templating a Ruby
  file around three checksums computed in a previous job. It is exactly the kind
  of fiddly, easy-to-get-subtly-wrong work a tool should do.
- A shell installer, a source tarball, a combined `sha256.sum`, and consistent
  artifact naming.
- An upgrade path: `dist generate` rewrites the workflow when the tool learns
  something new about GitHub Actions, rather than us discovering it at tag time.

`dist plan` produces, for this workspace:

```
hivemind-cli 0.1.0
  source.tar.gz          [checksum] source.tar.gz.sha256
  hivemind-cli-installer.sh
  hivemind.rb
  sha256.sum
  hivemind-cli-<target>.tar.xz
    [bin] hivemind
    [misc] CHANGELOG.md, LICENSE-APACHE, LICENSE-MIT, README.md
```

## What we lose

`dist` owns `release.yml` entirely — it is regenerated, and hand edits are
reverted. It has one hook that runs **before** the build (`github-build-setup`)
and several that run as **separate jobs** afterwards
(`local-artifacts-jobs`, `publish-jobs`, `post-announce-jobs`). Reading
`config/v0.rs` in 0.32.0, there is no hook that runs *after a binary is linked
and before it is packaged*, which is where `codesign -s -` has to go.

The pre-build hook covers the Node setup and the web bundle rebuild, so those
are kept. Ad-hoc codesigning is the thing that does not fit.

## How much that actually costs

Less than it first appears, and the difference is measurable rather than
assumed. On arm64 macOS the linker already ad-hoc signs every binary, because
arm64 macOS refuses to run an unsigned one. Checking a local build:

```
$ codesign -dv target/debug/hivemind
Format=Mach-O thin (arm64)
CodeDirectory v=20400 flags=0x20002(adhoc,linker-signed)
Signature=adhoc
```

`adhoc,linker-signed` is a stable code identity, which is all SPEC §13.4's
stated reason — "so the firewall prompt appears once" — needs. `dist` only
tars the binary, so the signature survives into the release artifact.

x86_64 macOS binaries get no automatic signature. So the real, narrow cost is:
**a user on an Intel Mac is asked about the firewall again after upgrading**,
where an Apple Silicon user is not.

## Decision

Adopt `dist`. Accept the x86_64 gap rather than keep a handwritten workflow that
cannot produce a Homebrew formula.

SPEC §13.4 is amended: the ad-hoc codesign requirement is met by the linker on
arm64 and is not met on x86_64. If that stops being acceptable, the options in
order of preference are:

1. Drop the x86_64 target. Intel Macs stopped shipping in 2023 and Rosetta runs
   the arm64 build.
2. Ask upstream for a post-build hook; the shape is uncontroversial.
3. Real signing via `macos-sign`, which needs an Apple Developer certificate —
   a different and much larger decision than this one.

## Consequences

- `release.yml` is generated. Do not edit it; edit `Cargo.toml`'s
  `[workspace.metadata.dist]` or `.github/build-setup.yml` and run
  `dist generate`.
- The formula is named `hivemind`, not `hivemind-cli`, so `brew install
  hivemind` works as SPEC §2 promises.
- `packaging/homebrew/` no longer holds a template to render. It keeps a README
  saying where the formula now comes from.
- Releasing needs the `dist` version pinned in `Cargo.toml` to match the one CI
  installs; `dist` checks this itself and fails the plan if they differ.
