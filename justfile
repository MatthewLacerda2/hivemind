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

# Same as `cov` but fails below the floor. CI switches to this in M1 (SPEC §14).
cov-gate:
    cargo llvm-cov nextest --workspace --all-features \
        --ignore-filename-regex '(tests/|crates/hivemind-cli/)' \
        --no-tests=warn --fail-under-lines {{COVERAGE_MIN}}

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

# Build the web UI into assets the binary embeds (SPEC §11).
web-build:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f web/package.json ]]; then
        echo "web: no UI yet — lands in M5 (SPEC §14)"
        exit 0
    fi
    cd web && npm ci && npm run build

# ------------------------------------------------------------------- all ----

# What CI runs, in CI's order. Run this before opening a PR (SPEC §16.6).
ci: fmt-check lint doc test deny openapi-check web-build

# `ci` plus the slow coverage and advisory passes.
ci-full: ci cov audit

release-dry-run:
    cargo dist plan
