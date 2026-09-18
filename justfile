# hivemind task runner (SPEC §13.3).
#
# `just ci` must run exactly what GitHub Actions runs. If you find yourself
# adding a step to a workflow that is not a recipe here, that is the bug.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Coverage floor for the crates that carry the logic (SPEC §13.2).
COVERAGE_MIN := "85"

_default:
    @just --list --unsorted

# ---------------------------------------------------------------- checks ----

# Format every crate.
fmt:
    cargo fmt --all

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all -- --check

# Clippy, pedantic, warnings are errors.
lint:
    cargo clippy --all-targets --all-features --workspace -- -D warnings

# Build the docs with warnings denied (a broken intra-doc link fails the build).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# ----------------------------------------------------------------- tests ----

# Everything except the tests that need real multicast.
test:
    # `--no-tests=warn`: a crate that genuinely has no tests yet (M0) should not
    # fail the run, but it should say so out loud every time.
    cargo nextest run --workspace --all-features --no-tests=warn
    cargo test --workspace --doc

# The #[ignore]d tests: real mDNS on a real network (SPEC §13.2).
test-network:
    cargo nextest run --workspace --all-features --no-tests=warn \
        --run-ignored all -E 'test(/network/)'

# Coverage report plus the gate on hivemind-core and hivemind-net.
cov:
    cargo llvm-cov nextest --workspace --all-features --no-tests=warn \
        --ignore-filename-regex '(tests/|crates/hivemind-cli/)' \
        --lcov --output-path lcov.info
    cargo llvm-cov report --summary-only \
        --ignore-filename-regex '(tests/|crates/hivemind-cli/)'

# What CI runs: same report as `cov`, and a failure below the floor.
cov-gate:
    cargo llvm-cov nextest --workspace --all-features --no-tests=warn \
        --ignore-filename-regex '(tests/|crates/hivemind-cli/)' \
        --lcov --output-path lcov.info \
        --fail-under-lines {{COVERAGE_MIN}}
    cargo llvm-cov report --summary-only \
        --ignore-filename-regex '(tests/|crates/hivemind-cli/)'

# -------------------------------------------------------------- supply chain ----

audit:
    cargo audit --deny warnings

deny:
    cargo deny --all-features check

# ------------------------------------------------------------------ docs ----

# The checked-in OpenAPI document must match what the code generates (SPEC §7).
openapi-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f docs/openapi.json ]]; then
        echo "openapi: nothing checked in yet — the document lands with the local router in M1 (SPEC §14)"
        exit 0
    fi
    generated="$(mktemp)"
    trap 'rm -f "$generated"' EXIT
    cargo run -q -p hivemind-cli --bin hivemind -- openapi --stdout > "$generated"
    diff -u docs/openapi.json "$generated"

# Did CI really run on this pull request's head? `just mergeable PR=12`
#
# The last thing to run before `gh pr merge`. "The checks look green" and "the
# checks ran" are not the same claim, and a loop that waits for checks to
# finish reads *absent* as settled — see the script's own docstring.
mergeable PR="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "{{PR}}" ]]; then
        echo "usage: just mergeable PR=12" >&2
        exit 2
    fi
    python3 .github/scripts/mergeable.py "{{PR}}"

# [gate] The scripts under .github/scripts. stdlib unittest, nothing to install.
scripts:
    python3 -m unittest discover --start-directory .github/scripts/tests --quiet

# Regenerate the changelog from conventional commits (SPEC §13.5).
# The v0.1.0 entry is hand-written and stays: there was no previous release to
# diff against, and a list of commit subjects is not a description of what the
# thing does. Everything after it is generated.
changelog tag="":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v git-cliff >/dev/null 2>&1; then
        echo "git-cliff is not installed: cargo install git-cliff --locked"
        exit 1
    fi
    if [[ -n "{{tag}}" ]]; then
        git-cliff --tag "{{tag}}" --unreleased --prepend CHANGELOG.md
    else
        git-cliff --unreleased --prepend CHANGELOG.md
    fi

# The release workflow is generated; fail if it has drifted (ADR 0011).
dist-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v dist >/dev/null 2>&1; then
        echo "dist: not installed — `cargo install cargo-dist --locked` to check the release workflow"
        exit 0
    fi
    dist generate --mode=ci --check

# Build the web UI into assets the binary embeds (SPEC §11).
web-build:
    #!/usr/bin/env bash
    set -euo pipefail
    cd web && npm ci && npm run check && npm run build
    # The built bundle is checked in because `include_dir` embeds it at compile
    # time: a clone without Node must still build the daemon. That makes it a
    # generated file in git, so CI fails if it has drifted from the source —
    # the same bargain `openapi-check` makes.
    cd .. && git diff --exit-code -- crates/hivemind-api/assets/hivemind.js

# ------------------------------------------------------------------- all ----

# What ci.yml runs, in its order — including the coverage gate, which runs as a
# separate job there. Run this before opening a PR (SPEC §16.6).
ci: fmt-check lint doc test scripts deny openapi-check web-build dist-check cov-gate

# `ci` plus what nightly.yml runs on a schedule.
ci-full: ci audit

release-dry-run:
    cargo dist plan
