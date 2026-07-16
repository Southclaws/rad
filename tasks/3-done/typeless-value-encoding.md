# `Value` encoding: typeless raw JSON, and the path to safer types

Status: todo — a design question with a small live footgun attached, not a bug
hunt. Worth settling before richer numeric types land.

## How a scalar `Value` is represented today

`Value` is typeless: the schema defines it (`rad/protocol/lir.schema.yaml`,
`$defs.Value`) as `format: raw` — "one raw JSON scalar: a string, a number, a
boolean, or null; it carries no type of its own, the binder types it from the
context it meets, preserving JSON number precision." There are two encodings of
that idea in the tree:

- **LIR/PIR wire** — Schemancer maps `format: raw` to a raw-JSON type, so
  `lirwire.Value` / the PIR relation bytes are `json.RawMessage`. Values are
  built through the `lirwire` constructors (`SetString`/`SetInt`/`SetFloat`/
  `SetBool`/`SetNull`/`SetAny`, and `LitOf`) and read back by decoding with
  `json.Number` (`rad/server/api/graphconv.go`) and coercing against the column
  type in the binder (`coerceLiteral`). `encoding/json` marshals a nil
  `json.RawMessage` as `null`, so this path cannot emit invalid JSON.
- **OpenAPI transport** (`rad/api`, ogen) — the `/execute` result and plan and
  `ColumnDefault.value` are ogen raw-JSON fields, wrapped by `anyToRaw` /
  `rawToAny` in `rad/api/convert.go`.

So a scalar is hand-wrapped on write and hand-parsed on read, on both sides, in
both layers. That is workable — `Value` genuinely can be any core scalar (text,
number, float, bool) — but it is a surface with no compile-time safety.

## The live footgun (ogen layer only)

An ogen raw-JSON field left zero-valued encodes **nothing**, producing invalid
JSON like `{"value":}` that fails on the *consumer's* decode, far from the
mistake. This is confined to the ogen types now (the `json.RawMessage` path is
safe): the mitigations are call-site discipline — `anyToRaw(nil)` emits explicit
`null`, and the `/execute` response sets `plan` to `null` when absent. Live
sites: `ColumnDefault.value`, the `/execute` result and plan.

Cheap lock: a `rad/api` test that constructs each raw-JSON-carrying oas type
zero-valued, encodes, and asserts valid JSON — documenting the invariant the
conversions must hold. Do this regardless of the larger design.

## The design question (the real weight)

Typelessness is fine for the four JSON scalars, but two forces push past it:

- **Safer, richer numerics.** Fixed-point decimal, arbitrary-precision decimal,
  and big integers — for arithmetic that does not silently lose precision or
  drift. JSON numbers cannot carry these losslessly.
- **JSON as the transport.** A JSON number decodes to an IEEE-754 double in most
  parsers, so an `int64` past 2^53 loses precision unless the client reads
  numbers as strings/bignums, and there is no decimal representation at all.
  The current wire does not scale to the types above.

One candidate direction: **encode every value as a string** on the wire —
`"0.333312"`, `"stringly"`, `"92"`, `"true"` — the string being the canonical,
lossless form, with the concrete type resolved from the column/context. This
future-proofs decimals, big integers, and new scalar types in a single move,
and removes the "JSON number is a double" hazard entirely. The cost is developer
ergonomics: every value (including plain numbers and booleans) arrives as a
string, so the generated client must parse and map it to the language-native
type per column — more work at the boundary, and a wire that reads less
obviously.

Open for discussion — no direction chosen. The `SetX`/`LitOf` helpers and
`decodeCell` are the seam any change flows through; a schema-level change to
`Value` (or a typed replacement) regenerates `lirwire`/`pirwire` and reshapes
both.

## Related

`rad/api/convert.go` (`anyToRaw`/`rawToAny`), `rad/protocol/lirwire` builders,
`graphconv` decode, the binder's `coerceLiteral`, and [[error-propagation]]
(typed per-class error metadata adds more raw-JSON fields — same discipline).
