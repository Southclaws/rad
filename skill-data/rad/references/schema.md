# Declarative schema reference

## Contents

- Stable identity
- Tables and columns
- Keys, indexes, and foreign keys
- Defaults and formats
- Current migration behavior
- Editing checklist

## Stable identity

Every table has a positive integer `id`. Every column has a positive integer `id` unique within its table. Names are labels; IDs carry identity across versions.

- Keep the ID and change the name to rename an object.
- Allocate a new unused ID to add an object.
- Never reuse a removed ID.
- Inspect the whole desired schema and accepted snapshot before selecting IDs.

Changing both name and ID represents removal of the old object and creation of another. Confirm the JSON diff reports the intended operation.

## Tables and columns

```yaml
# yaml-language-server: $schema=https://www.radengine.dev/rad.schema.json
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true, default: uuid(), format: uuid }
      - { id: 2, name: handle, type: string, unique: true }
      - { id: 3, name: created_at, type: int64, default: now_ms(), format: unix_ms }
```

Identifiers use lowercase ASCII letters, digits, and underscores, starting with a letter or underscore. Column types are:

- `string`
- `int64`
- `float64`
- `bool`

Set `nullable: true` only when `NULL` is valid. `format` is semantic metadata and does not validate stored values.

## Keys, indexes, and foreign keys

Use column shorthand for one-column definitions:

```yaml
- { id: 2, name: email, type: string, unique: true }
- { id: 3, name: team_id, type: string, ref: teams.id, index: true }
```

Use table forms for compound definitions:

```yaml
primary_key: [team_id, user_id]
indexes:
  - { columns: [team_id, role] }
  - { name: team_handle_uq, columns: [team_id, handle], unique: true }
foreign_keys:
  - name: membership_team_fk
    columns: [team_id]
    ref_table: teams
    ref_columns: [id]
```

Referenced columns must be the referenced table's complete primary key. Primary-key columns cannot be nullable.

## Defaults and formats

Literal defaults must match the column type. Generator defaults are:

- `uuid()` on `string`
- `now_ms()` on `int64`

A generator runs for new omitted writes. It does not backfill old rows merely because it is added.

## Current migration behavior

Immediate changes include creating or renaming tables, many column additions, renames, and default changes. Durable online work covers index builds, supported column representation changes, and nullable-to-required validation.

Expect rejection for unsupported structural changes including primary-key changes, foreign-key changes on existing tables, existing-column reorder, index rename, and replacement of a column used by a primary key or foreign key.

Deletion is logical first and physical reclamation is automatic. Deleting data-bearing tables or columns requires explicit data-loss consent.

Type conversions are strict: no whitespace or locale guessing, numeric narrowing must be exact, and values that cannot convert prevent publication. Inspect the full migration guide at https://www.radengine.dev/docs/schemas.

## Editing checklist

1. Read the desired schema and the accepted snapshot named by `rad.state/schema.lock.json` when present.
2. Preserve existing IDs and allocate unused IDs for additions.
3. Run `rad --output json validate`.
4. Run `rad --output json schema diff` against the intended server.
5. Check `changes`, `destructive`, and `blocking` separately.
6. Apply only after the plan matches the requested data model and any destructive consent is explicit.
7. Run `rad --output json schema status` and relevant application tests.
