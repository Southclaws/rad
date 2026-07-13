## post-JSONSchema logical validation is the right direction

I would treat validation as layered:

```text
1. JSON Schema validation
   - object shape
   - required fields
   - enum values
   - scalar types
   - no unknown properties

2. Graph validation
   - root node exists
   - every referenced node exists
   - no unreachable nodes
   - no cycles
   - every node has exactly one consumer, counting root/crossing/input references

3. Binding validation
   - scopes exist
   - columns exist
   - scopes are visible where referenced
   - join children cannot see each other
   - crossing correlation is legal
   - no duplicate output scopes where they would collide

4. Type/cardinality validation
   - predicate is bool-ish
   - arithmetic operands make sense
   - scalar crossings have one output column
   - aggregate/group legality
   - root cardinality rules

5. Normalisation/planning
   - decorrelation
   - predicate pushdown
   - projection pruning
   - common-subexpression detection
   - physical planning
```

JSON Schema gets you a valid **document**. Logical validation gets you a valid **query**.

That split is absolutely the right direction.
