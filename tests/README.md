# Integration tests

Lands in M1 (SPEC §13.2, §14), as a test-only workspace member.

These tests spin up two or three real daemons in temporary directories on random
ports, pair them, and exercise the behaviour that only shows up when whole
processes talk to each other:

- send and receive, reply threading, broadcast
- inline versus lazy attachments, including killing a blob transfer mid-way and
  asserting it resumes with a range request
- store-and-forward: stop the recipient, send, start the recipient, assert the
  mail lands
- rejection of unpaired senders, and idempotent redelivery
- index rebuild after deleting `index.db`
- hook installation merging into an existing `~/.claude/settings.json`

Unit tests live in their own crates. What goes here is specifically the things a
unit test cannot see.
