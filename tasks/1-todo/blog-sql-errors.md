https://sqlite.org/forum/info/0d4309b9c3cefb17f6bcc9e95fa66ce0ef533d823323d88b71de891ad870e788

I actually think this is an under-appreciated philosophy. People massively overestimate how much raw throughput matters and underestimate how much **developer friction** costs.

A lot of successful software has won by being _pleasant_ rather than _optimal_:

- Python over C/C++
- Go over Rust (for many companies)
- PostgreSQL over "faster" databases
- Git over faster VCSs (despite Git's UX being... Git)
- React over hand-written DOM code
- SQLite itself over many embedded databases

You spend thousands of hours _using_ a database. Saving 10 ms on a query is often worth less than saving 10 minutes debugging.

---

There are a lot of examples where existing databases optimize the engine at the expense of the human.

## 1. PostgreSQL's SQLSTATE errors

This is probably the biggest offender.

You get

```
ERROR: duplicate key value violates unique constraint "users_email_key"
DETAIL: Key (email)=(foo@example.com) already exists.
```

Actually not terrible.

Then you hit

```
ERROR: operator does not exist: text = integer
```

with

```
HINT: No operator matches the given name and argument types.
```

Great...

...except it doesn't point at the exact expression when it's buried in a 300-line query.

Modern languages give

```
line 17
column 42
expected string
found integer
```

SQL often just shrugs.

---

## 2. Foreign keys

Exactly what you mentioned.

SQLite:

```
FOREIGN KEY constraint failed
```

No idea which one.

Postgres is somewhat better:

```
insert or update on table "posts"
violates foreign key constraint "posts_author_id_fkey"
```

but then...

```
DETAIL: Key (author_id)=(123) is not present in table "users".
```

Still missing:

- row number
- statement location
- parameter
- original input object

Imagine instead

```json
{
  "statement": 3,
  "row": 17,
  "column": "author_id",
  "constraint": "posts_author",
  "value": 123
}
```

---

## 3. CHECK constraints

Postgres:

```
new row violates check constraint "users_age_check"
```

Cool.

What's the check?

```
age > 18
```

Maybe.

Maybe it's

```
(age > 18 AND country <> 'UK')
```

Maybe it's

```
CASE WHEN ...
```

It doesn't tell you.

It knows the expression.

It simply doesn't surface it.

---

## 4. NOT NULL

```
null value in column "email" violates not-null constraint
```

Which row?

Which JSON payload?

Which parameter?

No clue.

---

## 5. Type conversion

This one drives me insane.

```
invalid input syntax for type uuid
```

Where?

Which UUID?

Which column?

If there are 25 UUIDs...

Good luck.

---

## 6. Planner errors

One of my favourites.

```
column "name" must appear in the GROUP BY clause
```

Yes.

Which GROUP BY?

Which SELECT?

Which aggregate?

What expression caused it?

No source span.

No AST reference.

---

## 7. Ambiguous column

```
column reference "id" is ambiguous
```

Between...

```
users.id
```

and

```
posts.id
```

Maybe.

Tell me.

---

## 8. Cyclic FK

Often becomes

```
cannot delete because of foreign key constraint
```

Cool.

Show me the dependency graph.

You literally have it.

---

## 9. Duplicate aliases

```
table name specified more than once
```

Where?

```
FROM users u
JOIN users u
```

Highlight the second `u`.

---

## 10. Recursive CTE

Postgres:

```
recursive reference to query "foo" must not appear within its non-recursive term
```

If you're lucky.

Otherwise you spend 20 minutes staring at WITH RECURSIVE.

---

## 11. JSON

```
cannot extract element from a scalar
```

...

Which path?

Which scalar?

Which value?

---

## 12. SQL parser

My favourite class of errors.

```
syntax error at or near ")"
```

There are 700 parentheses.

Thanks.

---

# Why RAD is in a uniquely good position

This is what excites me about your architecture.

Because **LIR is structured**, every object has identity.

Instead of

```
ERROR near SELECT
```

you can say

```json
{
  "code": "unknown_column",
  "node": "project_7",
  "field": "full_name",
  "expression": "binary_12",
  "column": "surname"
}
```

or

```json
{
  "code": "duplicate_output_column",
  "project": "customer_projection",
  "column": "name",
  "existing_field": 2,
  "new_field": 5
}
```

or

```json
{
  "code": "non_deterministic_first",
  "crossing": "first",
  "node": "customer_posts",
  "reason": "input relation is unordered and may produce multiple rows",
  "suggestion": "insert an order node or prove uniqueness"
}
```

Those are errors that are **impossible** to express cleanly in SQL because the language is just text. By the time many engines detect the problem, they've thrown away the syntactic context or lowered it into internal structures that no longer correspond neatly to what the developer wrote.

RAD doesn't have that problem. The API _is_ the AST. Every planner transformation can preserve source provenance, node IDs, binding names, catalog IDs, and even generated code locations. That means you can make error reporting a first-class design goal rather than something bolted on afterwards.

I think this is one of the strongest UX arguments for an API-first relational database. It's not just that JSON is nicer than SQL strings—it's that the engine never has to lose the semantic information needed to explain _exactly_ what went wrong.
