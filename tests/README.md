# Integration tests

They live in [`crates/hivemind-cli/tests/`](../crates/hivemind-cli/tests/), not
here.

SPEC §13.2 asks for integration tests in `tests/` at the workspace root. Cargo
only sets `CARGO_BIN_EXE_hivemind` for integration tests belonging to the
package that *defines* the binary, and these tests spawn the real `hivemind`
daemon as a process — so a workspace-root crate would have to guess where the
binary was built, which breaks under `--target`, custom `CARGO_TARGET_DIR` and
`cargo llvm-cov`'s separate target directory.

The tests spawn real daemon processes rather than mounting the router
in-process, because what actually breaks in the field — the binary not
starting, the data directory not being created, the identity not persisting
across a restart — is invisible to an in-process test.

Covered so far (M1):

- a fresh daemon creates its data directory, identity and four mailboxes
- send, list and read a message through the CLI, end to end
- reading clears the unread count
- a reply lands in the same thread
- mail and node identity survive the daemon restarting
- deleting `index.db` loses nothing, because the files are the truth
- the CLI explains itself when no daemon is running

M3 grows this into two and three daemons: pairing, delivery, store-and-forward
with the recipient stopped, rejection of unpaired senders, and idempotent
redelivery.
