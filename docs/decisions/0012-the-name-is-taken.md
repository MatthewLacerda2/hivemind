# 12. `brew install hivemind` installs somebody else's program

Date: 2026-09-18

## Status

**Accepted: option A, provisionally.** The owner chose the tap so that the rest
of the work can finish, and intends to revisit convenience of installation
later. Everything below is kept because the reasoning does not expire — the
argument for B or C is the argument that will be read when it is revisited.

## What happened

SPEC §2 promises a three-line setup, and the first line is:

```
brew install hivemind
```

That command works. It installs
[DarthSim/hivemind](https://github.com/DarthSim/hivemind) — a process manager
for Procfile applications, version 1.1.0, already in Homebrew core.

This was found by doing what a newcomer would do: cloning the repository and
running the first line of the README. The result is worse than a command that
fails, because a command that fails says so. This one succeeds, installs
unrelated software, and the next line — `hivemind init` — then fails with a
confusing error from a program that was never meant to be here.

The collision is not only Homebrew. On crates.io, `hivemind` is taken by an
unrelated key-value store. And a binary called `hivemind` on `PATH` is
ambiguous on any machine that has the process manager installed: whichever
comes first in `PATH` wins, silently.

## Why it matters more than it looks

The project's reason to exist is a one-line setup. SPEC §2 says the users
"are competent but will not read docs", and §1 is about Claude Code instances
finding each other with no configuration.

So the failure is not cosmetic. **The most copy-pasteable line in the README
installs the wrong software**, and the person most likely to copy it is exactly
the person the project is for: somebody who was handed a link and told to run
it, including an agent reading the README on their behalf.

## What is actually available

Checked on 2026-09-18:

| Name | Homebrew core | crates.io |
|---|---|---|
| `hivemind` | taken (process manager) | taken (key-value store) |
| `hivemail` | free | free |
| `hivepost` | free | free |
| `mailbag` | free | free |
| `courier` | free | taken |
| `postbox` | taken | — |

## The three ways out

### A. Keep the name, install from a tap

`brew install MatthewLacerda2/tap/hivemind`.

- **For:** nothing is renamed. No code, no documents, no identity change. It
  works today — the release pipeline already publishes to that tap.
- **Against:** the setup line is longer and has to be remembered exactly.
  Anybody who types the short form gets the other program, and gets no warning
  that they did. The binary on `PATH` stays ambiguous for anyone who has both.
- **Honest summary:** cheapest, and leaves a trap in place for anyone who
  guesses.

### B. Keep the project name, rename what ships

The repository, the docs and the daemon stay `hivemind`; the Homebrew formula
and the binary become something free — `hivemail` is the closest.

- **For:** `brew install hivemail` is one line again and cannot install the
  wrong thing. The project keeps its name and its story.
- **Against:** two names for one thing, and that is a real cost — every
  instruction has to say which. "Install hivemail, then run `hivemail init`,
  and read the hivemind docs" is a sentence nobody enjoys.
- **Honest summary:** solves the trap, buys a small permanent confusion.

### C. Rename the project

Everything becomes one free name.

- **For:** one name everywhere, short setup line, no ambiguity on `PATH`, and
  the crates.io name is available too — which matters the day any of these
  crates is published.
- **Against:** the most work, though most of it is mechanical. It is also the
  only option that touches the *identity* of the thing, and the current name is
  a good one — "hivemind" says what it does.
- **Honest summary:** the only option with no residue, and the only one that
  costs something that cannot be got back.

## The decision, and why it is provisional

**A**, for now, chosen deliberately as a stopgap rather than as an answer. The
project is not published yet, so nobody can hit the trap today: the trap opens
the moment a release exists and somebody types the short form.

That is the trigger for revisiting this. Publishing without settling it is what
would be careless; deferring it while nothing is published is not.

## Recommendation, for when it is revisited

**B**, if the name matters; **C**, if it does not.

The reason to prefer B over A is that A's cost is paid by the person who guesses
wrong, silently, and that is the worst kind of cost — nobody who hits it will
report it, they will just think the project is broken.

The reason to prefer C over B is that two names is a tax on every sentence of
documentation forever, and this project's entire premise is that setup is
short.

A is the right answer only if this stays a handful of machines belonging to
people who were told the exact command.

## Consequences either way

SPEC §2 needs amending: its quick start is currently a promise that cannot be
kept. That happens in the same commit as whichever option is chosen.

Until a release exists, the README's quick start is `cargo install --path`,
which is what actually works and cannot install the wrong thing. It must not
say `brew install hivemind` under any option, including A — under A the command
is `brew install MatthewLacerda2/homebrew-tap/hivemind`, and the short form
stays wrong for ever.
