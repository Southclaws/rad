# Typeless `Value` fields can encode invalid JSON — make it impossible

Status: todo — a recurring footgun, hit twice now; currently held off by
call-site discipline only.

## The problem

The contract's `Value` schema is deliberately typeless ("an arbitrary JSON
value"), so ogen generates it as a raw-bytes alias (`jx.Raw`). For an
*optional* object property of that type, ogen cannot produce an `Opt`
wrapper — it emits a plain field and the generated encoder writes the field
**unconditionally**:

```go
{
    e.FieldStart("value")
    s.Value.Encode(e)   // zero jx.Raw encodes NOTHING
}
```

A zero raw therefore produces `{"func":"uuid","value":}` — invalid JSON
that fails on the *client's* decode, far from the bug. The error is
maddeningly indirect: `decode field "value": alias: skip: unexpected byte
125 '}'`.

Occurrences so far:

1. LIR `lit` exprs (fixed in oasconv, commit a1cb80c).
2. `ColumnDefault.value` (fixed in rad/api/convert.go `DefaultToOAS`,
   commit 0adf588): the conversion now always encodes a concrete raw —
   explicit `null` when the value is absent.

Both fixes are call-site discipline. The third occurrence will be written
by someone who doesn't know the rule.

## The rule (until fixed structurally)

Never leave a typeless `Value` field as a zero `jx.Raw`. Absent means
explicit `null` (`anyToRaw(nil)` produces it). Applies to every
conversion in rad/api and every handler constructing oas types directly.

## Candidate structural fixes (pick at build time)

1. **Guard test** (cheapest, do regardless): a rad/api test that reflects
   over — or simply enumerates — every generated type carrying a `Value`
   field, constructs it zero-valued, encodes, and asserts the output is
   valid JSON... which will *fail* today for zero values, documenting the
   invariant the conversions must maintain. Alternative shape: round-trip
   tests for each Value-carrying schema (`ColumnDefault`, query results,
   cells) including the absent-value case.
2. **Contract convention**: never declare an optional typeless property.
   Make `value` required with `null` as the absent sentinel, documented in
   the schema description (this is semantically what we do anyway).
   Enforceable by a schema lint over openapi.yaml.
3. **Upstream**: ogen could either wrap optional untyped fields in
   `OptValue` or encode zero raws as `null`. Worth an issue; not worth
   blocking on.

Option 2 makes the wire honest about what the code already does and kills
the class; option 1 catches regressions until then.

Related: rad/api/convert.go (`DefaultToOAS`), rad/protocol lit handling,
tasks/1-todo/error-propagation.md (the "value" of typed per-class meta will
add more Value-ish fields — apply the rule there).
