# Float-to-int range contract

Casting a non-finite or out-of-range float64 to int64 returns
`execution_failed`. The conversion must never saturate, wrap, or vary by
platform.

The fixture casts `1e19`, which is larger than `MaxInt64`. It permanently pins
the range check at the execution boundary and applies equally to interpreted,
planned, and future optimized execution paths.
