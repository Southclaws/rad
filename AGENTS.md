# Agent and contributor conventions

Conventions for anyone — human or agent — working in this repository. Keep
this short and enforceable; add a rule only when it has actually bitten.

## Go: prefer files over section separators

Do **not** delimit a file into sections with banner comments:

```go
// ── error and result mapping ──────────────────────────────────────────────
```

Reaching for a separator is the signal that the code beneath it is a distinct
concern that wants its own file. In Go, files are free: every file in a
directory shares the same package, so splitting costs nothing and keeps each
file to one concern. Create the file instead.

For example, the error and result-mapping helpers in the API server live in
`rad/server/api/errors.go`, not under a separator in `dbserver.go`.

When you encounter an existing banner separator, remove it and move the code
into an appropriately named file in the same package. This applies to the code
you touch; you need not sweep unrelated files in the same change.

(This is distinct from a short comment that documents the *next* declaration —
that is fine. The rule targets decorative section banners standing in for a
file boundary.)
