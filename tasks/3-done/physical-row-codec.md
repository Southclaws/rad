# Physical row codec: stop persisting the LIR wire datum

## The problem

Stored rows are persisted as `json.Marshal(map[columnID]lir.Value)`. A row on
disk looks like:

```json
{ "c2": {"type":"text","text":"6f48…"},
  "c3": {"type":"text","text":"Radlabs"},
  "c4": {"type":"int64","int64":1784639906616} }
```

The row-major grouping is *not* the problem — one KV item per logical row at
`/rad/data/{tableID}/primary/{pkTuple}` is the right shape (one read per PK
lookup, one atomic write per update, clean MVCC/conflict tracking later, scan
locality, no multi-key row reconstruction). Keep it.

The problem is that a schema-**independent** LIR wire datum (`lir.Value`) has
leaked across the executor boundary and become the schema-**directed**
persistent row encoding, collapsing three layers that must stay separate:

1. the LIR wire representation (a literal is self-describing because it has no
   column to supply its type);
2. the in-memory executor datum (`lir.Value`);
3. the physical persisted row.

A stored row is maximally schema-directed — `tableID + columnID → catalog
column → physical type` — so it should carry *no* type tags at all. Repeating
`"type":"int64"` in every cell of every row re-states what `c4` already
determines, and the JSON scaffolding (field names, quotes, braces) roughly
triples each row inside SlateDB's LSM (write amplification, block-cache
pressure, compaction).

PIR already models this boundary correctly (LIR is an opaque payload validated
separately, not merged into PIR). Physical storage deserves the same boundary:
the `codec` package owns the physical row format, and it is not `lir.Value`'s
JSON.

## No compatibility, no versioning

Zero users, no stored data anyone cares about. There is nothing to read old
rows *for*. So: one row format, replaced outright. No *version* discriminator,
no migration path, no per-row schema stamp — the single constant leading byte
is a corruption canary (below), not a format selector. If the format ever
changes again, it changes and dev databases are wiped. Do not build evolution
machinery for data that does not exist.

Reading existing JSON development rows may be kept as a throwaway convenience
*only* if it stays trivial; it is never a requirement and must not complicate
the binary format, the codec interface, or the catalog model. It is not a
compatibility surface.

## Keep

- One KV item per logical row; primary-key-addressed row keys.
- Stable table and column physical IDs.
- Column-ID-addressed, **sparse** row contents (a row omits columns added after
  it was written; those read back as their literal default, else NULL —
  generator defaults are never fabricated on read).
- The full row, including the primary-key columns, in the value (see PK
  section).
- The distinct, order-preserving key/index tuple codec (`keyenc`,
  `EncodeRowTuple`). It is already separate from the row-value codec and must
  stay so — row values optimize for compact schema-directed decoding; key
  tuples optimize for bytewise ordering and range scans. Never share them.

## Replace

- The JSON row encoding, the persisted `lir.Value`, and the "STORAGE CONTRACT
  / tags must never change" comment in `value.go` — delete that comment.
  `lir.Value` stays the in-memory executor datum; it is not a storage format.

## The binary row format

`MarshalRow` / `UnmarshalRow` in the `codec` package encode and decode a
schema-directed binary body. The KV value is a one-byte corruption canary
followed by that body.

```
byte     canary = 0x52  ('R')
uvarint  field count
repeat field-count times:
    uvarint  header  =  (columnDelta << 1) | nullFlag
    if not null:
        uvarint  payload length
        bytes    payload
```

Fields are sorted ascending by physical column ID; `columnDelta` is the
difference from the previous field's ID (the first field's delta is its own
ID). Folding the null flag into the header keeps a NULL field to a single
varint.

Payloads (the four scalar types), each **length-framed** even when
fixed-width:

- `bool`: one byte, `0x00` or `0x01`.
- `int64`: exactly 8 bytes, big-endian two's-complement.
- `float64`: exactly 8 bytes, big-endian IEEE-754 binary64 bits.
- `text`: UTF-8 bytes (the length prefix gives the count).
- NULL: null flag set, no length, no payload.

Big-endian for the fixed types is chosen for consistency with the
order-preserving key codec and readable diagnostic dumps; the value body needs
no lexical ordering, so the choice is about explicitness, not performance —
just never leave it implicit.

### Corruption canary

Every row value begins with a constant byte `0x52` — ASCII `'R'`. It is a
corruption canary, not a version discriminator: it never varies and is never
dispatched on. On read, the codec asserts the first byte is `0x52` before
decoding; a mismatch means the value is not a Rad row (a mis-constructed key
handed the decoder an index entry or catalog blob) or is corrupt, and it fails
fast and legibly instead of the framing parser marching into garbage.

The value is an easter egg with a real job: the server port `7237` is `RADS`
on a T9 phone keypad, and the canary is the `R` of RADS — printable, so every
stored row visibly opens with `R` in a `hexdump`. One byte is cheap; the
framing checks below catch most garbage regardless, so the canary is
belt-and-suspenders for the "wrong bytes reached the row decoder" class.

### Framing is what makes dropped columns safe

The row body supplies **traversal boundaries**; the catalog supplies
**interpretation**. This split is load-bearing: once a column is dropped its
bytes remain in existing rows, but the live catalog no longer knows their
type, so a purely type-directed unframed payload would be unskippable — the
decoder could not find the *next* field. Length-framing every present field
lets a reader skip an unknown/dropped column ID by its length without knowing
its type, while a known column ID is interpreted by its catalog physical type.
The one redundant length byte on a fixed-width field buys safe dropped-column
handling, unknown-field skipping, and corruption containment.

The codec reads only the live table — never revision history. `DeleteColumn`
removes a column from the live table outright, so a dropped field is
unrecognized and simply skipped, never surfaced; combined with never-reused
IDs, the reader needs nothing but the current schema.

A reader therefore:

- skips column IDs it does not recognize (dropped columns), using the length;
- interprets recognized column IDs by their catalog physical type;
- treats column IDs absent from the body (columns added after the row was
  written) as their literal default, else NULL.

## Immutable physical type, never-reused IDs, no per-row schema version

A stored field carries no type and no schema version, so bytes are interpreted
purely by physical column ID against the live table. Two catalog invariants
make that safe:

- **A physical column ID has one immutable physical scalar type for its entire
  lifetime.** A field tagged ID 7 decodes as its type — say `int64` — under
  every revision, whatever the column has been renamed to and whatever its
  nullability, default, or format has become. A logical type change requires a
  *new* physical column ID and explicit value migration, never reinterpreting
  bytes in place. Rad has no `alter_column_type` today; the invariant is
  recorded now so a future implementation cannot reinterpret bytes.
- **Physical column IDs are never reused** — `NextPhysicalID` is monotonic and
  never returns IDs to a pool. A dropped column's ID can never be reborn as a
  different-typed column, so a stale field left in an old row can never collide
  with a live column of another type.

Together these are the misinterpretation guard for *recognized* IDs; framing
(below) is the traversal guard for *unrecognized* ones. Neither needs a
schema-version stamp on the row: rows are not carried under, mapped to, or
decoded against the revision that wrote them.

## Present-NULL vs. absent

Three logical states, all of which must round-trip:

| Stored state               | Meaning                                                              |
| -------------------------- | ------------------------------------------------------------------- |
| field absent               | column not physically written; apply default, else NULL             |
| field present, null flag   | explicit NULL; never apply a default                                |
| field present with payload | the stored datum                                                    |

Representing NULL by omission would collapse the first two and silently
substitute a default for an explicit NULL. The null-flag-in-the-header encoding
keeps them distinct: explicit NULL writes a field (header, flag set, no
payload); an absent column writes nothing. Row-update merge logic must not turn
a present-NULL field into deletion from the sparse set — that would demote an
explicit NULL back to absent.

## Column identity

Tag fields with the **physical column ID** — the integer behind `col.ID`
(`"c" + N`, allocated from the monotonic `NextPhysicalID` counter, unique for
the database lifetime, present in both catalog modes). It is the storage
identity the row codec already uses; it gives metadata-only renames (cells key
on identity, not name), sparse rows after a column is added, harmless orphaned
fields after a column is deleted, independence from catalog display order,
skip-unknown readers, and corruption diagnostics.

Give it its own codec type — `PhysicalColumnID` (uint64, matching the
`NextPhysicalID` counter width; truncating the identity is unacceptable) — so
nobody later passes a PIR `SchemaID` merely because both are integers. Do not
key on `SchemaID` or the column name. Either expose the integer directly from
the catalog or parse it off the `"c"` prefix — do not invent a new identity.

Encode requires and decode enforces a canonical form: fields strictly ascending
by physical ID, no duplicates, delta greater than zero after the first field.
Canonical encoding matters for diagnostics, reproducibility, and fuzzing.

## Primary key in the value

Encode the full row, PK columns included, so the value decodes to a complete
logical row on its own: point reads and scans share one decoder, and admin/debug
tooling inspects a value without touching the key. The cost is that the PK is
also in the KV key (as an order-preserving tuple), so its payload is stored
twice.

Omitting PK columns from the value — reconstructing them from the key tuple on
read — is a possible later change if that duplication is ever shown to matter.
It trades storage for coupling: every read must decode key *and* value, row
materialization must rebuild PK columns from the key, and tooling can no longer
read a value alone. Once the body is compact the duplication is usually minor,
so encode the full row and leave PK-omission until measurement justifies it.

## `format: uuid` stays physically text

`format: uuid` is semantic metadata on a `text` column, not a physical type.
The codec encodes it as `text` and must never silently reinterpret a semantic
format as a narrower physical encoding — a secret 16-byte physical subtype
invisible to the type system would complicate casts, comparison, key encoding,
clients, and validation. A real UUID scalar gets its own storage encoding if
Rad ever adds one.

## Decode validation — reject

- a leading byte that is not the `0x52` canary (not a Rad row, or corrupt);
- zero or overflowing column deltas;
- duplicate or non-ascending column IDs;
- field-count mismatch, and trailing bytes after the declared field count;
- truncated varints or truncated payloads;
- a known fixed-width field whose framed length is wrong (bool ≠ 1, int64 ≠ 8,
  float64 ≠ 8);
- a `bool` payload other than `0x00` / `0x01`;
- invalid UTF-8 in a `text` payload (Rad's `text` contract is UTF-8).

Do **not** reject a present-NULL field on a currently-non-nullable column at
decode: nullability can tighten over a column's life, and a decode-time reject
would make previously-valid stored rows unreadable. NOT-NULL enforcement is a
mutation-time and tightening-migration concern, not the row reader's.

## Tests

- Round-trip per type: text (empty, ASCII, multi-byte UTF-8), int64
  (min/max/zero/negative), float64 (negative, fractional, ±0), bool.
- Present-NULL vs. absent, the load-bearing matrix:
  - added column with a literal default, old row with the field absent → default;
  - a new row that explicitly writes NULL → NULL (not the default);
  - a new row that writes a value → the value;
  - decode then re-encode an explicit NULL → still a present-NULL field.
- Dropped-column skip: a row containing a since-dropped column ID decodes (the
  orphaned field is skipped by its length) and the following fields still read.
- Canonical-form rejection: non-ascending / duplicate / zero-delta bodies, and
  each entry in the reject list above.
- Integration through the harness / e2e path so the real write → scan → read
  cycle exercises the codec. (`refexec` is an in-memory oracle and never
  touches storage, so the codec needs its own tests rather than differential
  coverage.)

## Non-goals

- A *version* discriminator byte, codec versioning, per-row schema-version
  mapping, and any intermediate framed-JSON format — there is no data to be
  compatible with. (The leading `0x52` byte is a constant corruption canary,
  not a version selector. Reading old JSON dev rows is allowed only as a
  trivial throwaway, per the policy above.)
- Varint (or zigzag) int64 in row values — fixed 8-byte, big-endian.
- Dropping the PK from the value.
- A real UUID (or any new) physical scalar type.
- Order-preserving row-value encoding (that is the key codec's concern).
- Row-value compression, and MVCC/versioning (their own designs).
