# Architecture decision records

One file per decision, `NNNN-kebab-title.md`, never edited once accepted —
superseded records get a new number and a `Superseded by` line.

A record may gain a dated addendum reporting how the decision fared once it
was used, as [0010](0010-tls-admits-strangers-the-application-rejects-them.md)
has. That is a report on the argument, never a revision of it: a decision that
turns out to be wrong is superseded, not quietly rewritten.

Records 0001–0005 were written before any code and encode the decisions already
made in `SPEC.md`. Anything after that is a decision the spec did not make, or
one it made and we found to be wrong: when the spec turns out to be wrong or
impossible, the process is to stop, write a record proposing the change, and ask
(SPEC §16).

Use `0000-template.md` as the starting point.
