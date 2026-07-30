# Int64 arithmetic overflow contract

Rad uses checked int64 arithmetic. `add`, `sub`, `mul`, `div`, and unary
`negate` return `execution_failed` when their mathematical result is outside
the int64 range. This includes `negate(MinInt64)` and `MinInt64 / -1`.

The fixture evaluates `9223372036854775807 + 1`. It must never return a wrapped
value or vary by platform. The same invariant applies to every integer
arithmetic path, including constant folding and future optimizer rules.
