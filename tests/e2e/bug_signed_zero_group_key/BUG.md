# BUG: signed zero split one numeric group into two

> **STATUS: FIXED** (2026-07-29). Found and automatically shrunk by the
> retained generated semantic campaign at seed `30445132973` in
> [run 30444132913](https://github.com/Southclaws/rad/actions/runs/30444132913).

Rad numeric equality treats IEEE `-0.0` and `+0.0` as equal. The physical
aggregate executor previously grouped scalar values by their order-preserving
storage tuple bytes. That encoding deliberately orders negative zero before
positive zero, so a count over `[-0.0, +0.0]` incorrectly produced two groups
with count one instead of one group with count two.

Semantic key tuples now canonicalize every zero to positive zero before
encoding. The shared boundary also keeps correlated key grouping, primary and
secondary key identity, and uniqueness enforcement consistent with numeric
equality. Row bodies still preserve the authored float bits; only semantic key
identity is canonicalized.
