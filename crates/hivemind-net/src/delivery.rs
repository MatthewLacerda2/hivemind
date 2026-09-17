//! Store-and-forward delivery.
//!
//! Sending writes to `mail/out/` and returns; a worker per recipient walks the
//! address book in `last_ok` order and retries with jittered exponential
//! backoff from 2 s to 5 min, forever (SPEC §8).
