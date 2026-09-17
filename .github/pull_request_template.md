## What this does

<!-- One paragraph. What changes for a user of hivemind? -->

## Spec sections

<!-- Which parts of SPEC.md does this implement? e.g. §4.1, §7.1. If this
     diverges from the spec, link the ADR that proposes the change — divergence
     without a record is not reviewable (SPEC §16). -->

- Implements: §
- Milestone: M

## How it was verified

<!-- `just ci` passing is the floor, not the answer. What did you actually
     exercise, and what would have caught it if it were wrong? -->

- [ ] `just ci` passes locally
- [ ] New behaviour has tests whose names describe the behaviour (SPEC §13.2)
- [ ] Public API of `hivemind-core` is documented
- [ ] `docs/openapi.json` regenerated if endpoints changed
- [ ] New dependencies justified below

## New dependencies

<!-- Crate, version, why, and what maintenance shape it is in (SPEC §15).
     "None" is a great answer. -->
