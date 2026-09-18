# Homebrew packaging

Nothing to render here any more. `dist` generates `hivemind.rb` at release time
with the right per-architecture URLs and checksums and pushes it to the tap at
`MatthewLacerda2/homebrew-tap`, which is what `brew install hivemind` reads.

See `docs/decisions/0011-cargo-dist-generates-the-release.md` for why, and for
the one thing adopting `dist` cost us.

To change the formula's name or the tap, edit `[workspace.metadata.dist]` in the
workspace `Cargo.toml` and run `dist generate`. Do not edit
`.github/workflows/release.yml` — it is generated and your changes will be
reverted the next time anybody runs that command.
