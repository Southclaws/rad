# Remove Call scalar operation expression

I'd distinguish between **language features** and **engine primitives**.

Today, your engine primitives are:

- literals
- columns
- unary operators
- binary operators
- casts
- crossings

Those are fundamental pieces of relational evaluation.

`call` is different. It isn't fundamental—it's an extension mechanism.

## What do you actually lose?

Very little for the POC.

You lose things like:

```sql
LOWER(name)
ABS(balance)
CONCAT(...)
SUBSTR(...)
```

But those aren't needed to prove your architecture.

You can always add:

```yaml
CallExpr:
  kind: call
  fn: string
  args: [...]
```

later without breaking the conceptual model.

That's important: **adding a new `Expr` variant is an additive change**. Nothing else in the LIR has to change.

---

## It actually simplifies the binder

Without `call`, every expression is one of a handful of known cases.

Instead of:

```go
switch expr.Kind {
case Lit:
case Col:
case Unary:
case Binary:
case Cast:
case Call:
...
}
```

you have

```go
switch expr.Kind {
case Lit:
case Col:
case Unary:
case Binary:
case Cast:
case Exists:
case Scalar:
...
}
```

No function registry.

No overload resolution.

No argument count checking.

No volatility rules.

No deterministic/non-deterministic annotations.

No return type lookup.

No implicit casts.

That's a _lot_ of machinery you don't need yet.

---

## It also keeps the planner cleaner

Suppose you have

```text
call("equals", a, b)
```

versus

```text
binary(eq, a, b)
```

The latter immediately tells the planner

- this is equality
- usable for joins
- usable for indexes
- usable for transitive closure
- usable for predicate pushdown

With `call`, you'd need special cases anyway:

```go
if fn == "eq" {
    ...
}
```

So you're not actually gaining abstraction.

---

## My only suggestion

I wouldn't remove it forever.

I'd literally leave a comment in the schema:

```yaml
# Reserved for future extensible scalar functions.
# Omitted from the initial LIR until user-defined and built-in function
# semantics are designed.
```

or even keep a TODO in the design doc:

> Future LIR versions may introduce `CallExpr` for extensible scalar functions. The initial LIR deliberately exposes only closed primitive operators.

That signals that the omission is intentional rather than an oversight.

---

## Longer term

Eventually I suspect you'll want `call`.

Not just for SQL compatibility, but because your own frontends will eventually want things like

- string functions
- date functions
- JSON functions
- vector functions
- user-defined functions
- extension libraries

At that point, `call` becomes the generic escape hatch.

But I don't think you need to carry that abstraction through the binder, optimiser, and executor while you're still proving the core relational engine.

I'd rather see the first version of RAD support a rock-solid algebra with six expression types than an extensibility mechanism that isn't exercised yet. That's very much in keeping with the "small core, grow conservatively" philosophy you've been following.
