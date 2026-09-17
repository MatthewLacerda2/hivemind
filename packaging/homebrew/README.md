# Homebrew packaging

`hivemind.rb` is generated at release time (M6) by the release workflow, which
fills in the version, the per-architecture tarball URLs and their SHA-256
checksums from the artifacts it just built.

Writing the formula by hand now would mean checking in a version and three
checksums that are wrong until the first tag, which is worse than not having the
file: a stale formula looks installable.

The eventual home is a tap repository (`MatthewLacerda2/homebrew-tap`) so that
`brew install hivemind` works as SPEC §2 promises. Until then this directory
holds the template the release job renders.
