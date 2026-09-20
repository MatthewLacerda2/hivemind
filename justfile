# hivemind task runner (SPEC §13.3).
#
# `just ci` must run exactly what GitHub Actions runs. If you find yourself
# adding a step to a workflow that is not a recipe here, that is the bug.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Coverage floor for the crates that carry the logic (SPEC §13.2).
COVERAGE_MIN := "85"

# What the coverage gate does not measure: the integration tests themselves,
# and the CLI, whose behaviour the integration tests cover end to end.
COV_IGNORE := '(tests/|crates/hivemind-cli/)'

# A long recipe with nothing to say should not spend a session's context saying
# it (#71). `quiet.py` runs a command, prints one line when it passes and the
# whole of its output when it does not, and never touches the exit status.
# `--verbose` on any recipe that mentions QUIET streams the lot instead.
QUIET := "python3 .github/scripts/quiet.py"

_default:
    @just --list --unsorted

# ---------------------------------------------------------------- checks ----

# [gate] Is this machine fit to believe a green run from? Refuses a shared
# CARGO_TARGET_DIR (#46); warns when disk is low (#61).
workspace:
    python3 .github/scripts/workspace.py

# Format every crate.
fmt:
    cargo fmt --all

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all -- --check

# Clippy, pedantic, warnings are errors. `just lint --verbose` for every line.
lint *ARGS:
    @{{QUIET}} {{ARGS}} --label lint -- \
        cargo clippy --all-targets --all-features --workspace -- -D warnings

# Build the docs with warnings denied (a broken intra-doc link fails the build).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# ----------------------------------------------------------------- tests ----

# `--no-tests=warn`: a crate that genuinely has no tests yet (M0) should not
# fail the run, but it should say so out loud every time.
#
# `--tail 1` keeps nextest's summary line, which counts the tests that ran.
# "the suite passed" and "the suite was empty" are different states, and that
# line is the only place the difference shows (#71).

# Everything except the tests that need real multicast. `--verbose` for the lot
test *ARGS:
    @{{QUIET}} {{ARGS}} --label test --tail 1 -- \
        cargo nextest run --workspace --all-features --no-tests=warn
    @{{QUIET}} {{ARGS}} --label doctests --tail 1 -- \
        cargo test --workspace --doc

# The #[ignore]d tests: real mDNS on a real network (SPEC §13.2).
test-network:
    cargo nextest run --workspace --all-features --no-tests=warn \
        --run-ignored all -E 'test(/network/)'

# Coverage report plus the gate on hivemind-core and hivemind-net.
cov:
    cargo llvm-cov nextest --workspace --all-features --no-tests=warn \
        --ignore-filename-regex '{{COV_IGNORE}}' \
        --lcov --output-path lcov.info
    @just _cov-table

# The per-file table, from the profile data already on disk. CI's coverage
# summary prints the same table, and a copy of the expression there is a second
# place for it to be wrong.
_cov-table:
    cargo llvm-cov report --summary-only \
        --ignore-filename-regex '{{COV_IGNORE}}'

# The per-file table is `just cov`'s, where somebody has asked to read it. This
# one is asked twenty times a session and the answer wanted is the number (#71).

# [gate] What CI runs: the coverage floor, and the number it held at
cov-gate *ARGS:
    @{{QUIET}} {{ARGS}} --label cov-gate -- \
        cargo llvm-cov nextest --workspace --all-features --no-tests=warn \
        --ignore-filename-regex '{{COV_IGNORE}}' \
        --lcov --output-path lcov.info \
        --fail-under-lines {{COVERAGE_MIN}}
    @python3 .github/scripts/coverage.py --floor {{COVERAGE_MIN}} -- \
        cargo llvm-cov report --json --summary-only \
        --ignore-filename-regex '{{COV_IGNORE}}'

# -------------------------------------------------------------- supply chain ----

audit:
    cargo audit --deny warnings

deny:
    cargo deny --all-features check

# ------------------------------------------------------------------ docs ----

# [gate] The checked-in OpenAPI document must match what the code generates
openapi-check *ARGS:
    @{{QUIET}} {{ARGS}} --label openapi-check -- just _openapi-check

# The check itself (SPEC §7). Exits 79 when there is nothing to compare, which
# the wrapper reports as `skip` rather than `ok` and then turns back into a 0:
# a gate that declined has not passed, and a gate that declined has not failed
# either.
_openapi-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! -f docs/openapi.json ]]; then
        echo "openapi: nothing checked in yet — the document lands with the local router in M1 (SPEC §14)"
        # 79, not 0: nothing was compared, and a summary line that said `ok`
        # would be claiming otherwise. See quiet.py's docstring.
        exit 79
    fi
    generated="$(mktemp)"
    trap 'rm -f "$generated"' EXIT
    cargo run -q -p hivemind-cli --bin hivemind -- openapi --stdout > "$generated"
    diff -u docs/openapi.json "$generated"

# The last thing to run before `gh pr merge`. "The checks look green" and "the
# checks ran" are not the same claim, and a loop that waits for checks to
# finish reads *absent* as settled — see the script's own docstring.

# Did CI really run on this pull request's head? `just mergeable 12 [--wait]`
mergeable PR="" *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "{{PR}}" ]]; then
        echo "usage: just mergeable 12 [--wait] [--timeout=SECONDS]" >&2
        exit 2
    fi
    python3 .github/scripts/mergeable.py "{{PR}}" {{ARGS}}

# [gate] The scripts under .github/scripts. stdlib unittest, nothing to install.
scripts:
    python3 -m unittest discover --start-directory .github/scripts/tests --quiet

# A signal, and the nightly workflow is where it lives properly — this is for
# running one by hand while the code that failed is still in front of you.
# Seeds are in `fuzz/seeds/` and checked in; without them the fuzzer spends its
# budget discovering that random bytes are not JSON. Passing both directories
# is what makes the difference: 159 lines of coverage from nothing, 971 from
# the seeds.

# Run the fuzz targets for a short budget (SPEC §13.2). Needs nightly
fuzz target="" seconds="60":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-fuzz >/dev/null 2>&1; then
        echo "fuzz: not installed — cargo install cargo-fuzz --locked" >&2
        exit 1
    fi
    targets="{{target}}"
    if [[ -z "$targets" ]]; then
        targets=$(cd fuzz && cargo +nightly fuzz list)
    fi
    for t in $targets; do
        echo "── $t"
        mkdir -p "fuzz/corpus/$t"
        seeds=""
        [[ -d "fuzz/seeds/$t" ]] && seeds="fuzz/seeds/$t"
        cargo +nightly fuzz run "$t" "fuzz/corpus/$t" $seeds -- \
            -max_total_time={{seconds}}
    done

# A ratchet, not a target: the limits start just under the largest file that
# exists and only ever come down. `just size --report` lists what to split next.

# [gate] No Rust file over its line-of-code limit. Comments and blanks are free
size *ARGS:
    python3 .github/scripts/size.py {{ARGS}}

# [gate] The layering SPEC §3.1 and §10 describe, held to rather than hoped for.
boundaries:
    python3 .github/scripts/boundaries.py

# A marker naming work that already happened is a lie in a file somebody
# trusts, and nothing reads it, so nothing complains. This complains.

# [gate] A `TODO(Mn)` whose milestone has already shipped
markers:
    python3 .github/scripts/stale_markers.py

# The automatic form of something done by hand through M1-M6 — when a test
# passes first time, break the code and check the test fails. Scoped to this
# branch's diff against `main`, because the whole workspace is 1234 mutants and
# nobody is going to sit through that. Do not run it while something else is
# compiling; it fans out.

# Which changes to the code no test would notice. A signal: blocks nothing
mutants *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-mutants >/dev/null 2>&1; then
        echo "mutants: not installed — cargo install cargo-mutants --locked" >&2
        exit 1
    fi
    if ! git rev-parse --verify --quiet origin/main >/dev/null; then
        echo "mutants: origin/main is not in this clone, so there is no diff" >&2
        echo "         fetch it with: git fetch origin main" >&2
        exit 1
    fi
    # The longest unattended thing here, and the one that fills a disk while
    # nobody is watching: it copies the tree per job. Ask first (#61).
    python3 .github/scripts/workspace.py --quiet
    mkdir -p target
    git diff origin/main...HEAD -- '*.rs' > target/pr.diff
    if [[ ! -s target/pr.diff ]]; then
        echo "mutants: this branch changes no Rust source against origin/main."
        exit 0
    fi
    # `--in-diff` keeps the cost proportional to the change. A survivor here is
    # a gap in what this branch wrote, which is the only kind worth acting on
    # while the code is still in hand.
    cargo mutants --in-diff target/pr.diff {{ARGS}}

# The v0.1.0 entry is hand-written and stays: there was no previous release to
# diff against, and a list of commit subjects is not a description of what the
# thing does. Everything after it is generated.

# Regenerate the changelog from conventional commits (SPEC §13.5)
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

# [gate] release.yml is generated by `dist`; fail if it has drifted (ADR 0011)
dist-check *ARGS:
    @{{QUIET}} {{ARGS}} --label dist-check -- just _dist-check

_dist-check:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v dist >/dev/null 2>&1; then
        # Single quotes: backticks inside a double-quoted string are command
        # substitution, so this advice used to *run* `cargo install` on a
        # machine that did not have dist — found while testing the skip (#71).
        echo 'dist: not installed — cargo install cargo-dist --locked to check the release workflow'
        # 79 rather than 0: a check that declined to run has not passed (#71).
        exit 79
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
# separate job there. Run this before opening a PR (SPEC §16.6). This list and
# ci.yml's steps are the same list; if they drift, that is the bug (SPEC §13.3).
#
# `workspace` is first and deliberately so: it is the one gate whose failure
# makes every other gate's answer meaningless, and it costs milliseconds.
CI_GATES := "workspace fmt-check lint size boundaries markers doc test scripts deny openapi-check web-build dist-check cov-gate"

# The gates that already wrap their own work in quiet.py, because they are worth
# running on their own too. They are streamed rather than summarised again: their
# line carries the number — the count of tests that ran, the coverage percentage
# — and a second `ok` printed over the top would throw that away.
CI_REPORTING := "lint test openapi-check dist-check cov-gate"

# Every gate, in ci.yml's order: one line each, everything from what failed
ci *ARGS:
    @{{QUIET}} {{ARGS}} --reporting {{CI_REPORTING}} --gates {{CI_GATES}}

# `ci` plus what nightly.yml runs on a schedule.
ci-full: ci audit

# Remove the target/ of every worktree whose branch has already merged.
#
# The rule "remove a worktree the moment its branch merges" is held by
# remembering it, which is the weakest kind of rule — and when it is
# forgotten, the bill arrives as a confusing build error rather than as "no
# disk" (#61). Only merged branches, and never this worktree's own target/:
# the one you are standing in is the one you are about to need.
reap:
    #!/usr/bin/env bash
    set -euo pipefail
    here="$(git rev-parse --show-toplevel)"
    freed=0
    while read -r path _ branch; do
        branch="${branch#\[}"; branch="${branch%\]}"
        [[ "$path" == "$here" ]] && continue
        [[ -d "$path/target" ]] || continue
        # `main` is not merged into itself, and a worktree on it is somebody's
        # workspace rather than a leftover.
        [[ "$branch" == "main" ]] && continue
        if git branch --merged main --format='%(refname:short)' | grep -qx "$branch"; then
            size=$(du -sh "$path/target" 2>/dev/null | cut -f1)
            rm -rf "$path/target"
            echo "reaped $size from $path ($branch, merged)"
            freed=$((freed + 1))
        fi
    done < <(git worktree list)
    if [[ "$freed" == "0" ]]; then
        echo "nothing to reap — no merged worktree is holding a target/"
    fi

release-dry-run:
    cargo dist plan
