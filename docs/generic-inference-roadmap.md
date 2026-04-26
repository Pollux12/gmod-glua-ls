# TS-Like Generic Inference Roadmap

Goal: keep moving Lua generic inference toward TypeScript/TypeScript-Go behavior, but only by focused, test-backed steps.

Current status: generic inference phase 1 is effectively closed. The useful TS-like behavior for this project is mostly in place; future generic work should start from a concrete failing Lua-expressible case.

## Phase 1 Summary

The first TS-like generic inference phase is complete enough to stop and move to another target.

Implemented behavior:

- Scoped generic template ids, so nested class/function/alias generics do not collide by position.
- `InferenceContext` now owns candidate collection, priorities, variance, constraints, and finalization.
- `TypeSubstitutor` is now mostly a fixed-substitution mapper/backend, not the public inference engine.
- Candidate priorities model TS-like direct, contextual-return, homomorphic mapped, partial homomorphic mapped, mapped constraint, and naked-union fallback inference.
- Direct repeated candidates follow the TS-Go candidate-list/common-supertype shape:
  - preserve leftmost candidate on conflicts;
  - do not synthesize unrelated nominal ancestors;
  - combine only where the priority implies combination.
- Contextual return inference supports direct bindings, assignments, member/table-field contexts, overloads, union callables, metatable `__call`, and delayed callback parameters.
- Function inference has co/contra candidate buckets:
  - returns infer covariantly;
  - parameters infer contravariantly;
  - mixed co/contra selection follows TS-Go's `preferCovariantType` rule shape.
- Reverse mapped inference supports common Lua-expressible TS shapes:
  - homomorphic mapped tables;
  - `Pick<T, K>`-style key constraints;
  - mapped value fallback `{ [P in K]: V }`;
  - optional fields;
  - tuple and array sources;
  - partial mapped candidates at weaker priority.
- Conditional `infer` supports TS-like repeated candidates, source/pattern union matching, structural preference over naked fallback, and generic type-parameter constraints such as `Pair<T, U: T>`.
- Generic constraints are applied during finalization:
  - invalid speculative candidates fall back to the constraint;
  - contextual-return unions can be filtered by the constraint;
  - dependent constraints such as `U: T` can force co/contra fallback.
- Same-list generic constraints now see later params, e.g. `T: U, U`.
- Inline comments now mark the TS-Go-inspired inference rules without copying TS-Go comments verbatim.

## Architecture State

Current architecture is intentionally TS-inspired, not a full TS engine port.

Present:

- `InferenceContext` owns active inference state.
- Explicit APIs are used for candidate inference and fixed substitutions.
- Candidate priority and variance are explicit.
- Constraints are stored and applied during finalization.
- Pure instantiation still takes `&TypeSubstitutor`.

Not present yet:

- Full TS mapper split: `mapper`, `nonFixingMapper`, `returnMapper`, backreference mapper.
- Full TS inference flags/signature/compareTypes model.
- Full TS conditional type engine.
- Full TS relation/comparer/freshness model.

Decision: the full mapper split is deferred until a focused failing case proves the local adaptations are not enough.

## Relation Engine Boundary

Relation-engine work is separate from the generic inference phase.

Out of scope for phase 1:

- real freshness model;
- fresh vs non-fresh table literal tracking;
- excess-property architecture;
- broad `table` / open object behavior;
- `isKnownProperty`-style member lookup;
- structural relation cleanup beyond what directly blocks generic inference.

Current side step:

- Fresh table literal excess checks were narrowed to Lua-compatible TS-like behavior.
- Normal `---@class` targets stay open for Lua dynamic extension.
- Anonymous structural object targets and `(exact)` class targets keep fresh literal excess checks.
- Non-fresh expressions use a no-excess relation path.
- Superclass checks do not treat derived-class fields as excess.
- Computed/index-like members now follow the TS split more closely:
  - `LuaMemberKey::ExprType` is not treated as a required property;
  - matching actual keys and index signatures are still checked against its value type;
  - this mirrors the TS-Go separation between normal properties and `IndexInfo`.
- Current workspace `glua_check` on `/workspaces/gmod/garrysmod` no longer reports the options `param-type-mismatch` false positive; the current local result is 4 warnings and 200 hints.

Later: relation-engine work should become a separate refactor with a real freshness/index-info model, not part of the first generic inference phase.

## Next Generic Work

Generic refactoring should resume only from a focused failing test in one of these areas:

- a Lua-expressible case needing a real `nonFixingMapper` / `returnMapper` split;
- contextual-return priority leaks outside delayed closure/callback paths;
- callable/member-assignment/overload contextual inference edge not already covered;
- adjacent variadic/rest split gaps that Lua syntax can express cleanly;
- mapped/reverse-mapped edge where current priority or source reconstruction loses precision.

If no focused failing case exists, consider phase 1 closed and move to another target.

## Suggested Next Project Stage

Best next stage: relation-engine/freshness model.

Reason:

- Generic inference is now mostly TS-like for current Lua needs.
- Remaining warnings/regressions are more likely relation/assignability/freshness issues than generic inference issues.
- The recent computed/index-like member fix is intentionally scoped; a cleaner long-term design would model TS-like index signatures explicitly instead of letting them live as ordinary members.
- TS-like generic behavior will be easier to trust once assignability and excess/freshness behavior is more principled.

Alternative next stage: audit `/workspaces/gmod/garrysmod` on the current checker and classify any new diagnostics against the new generic/relation behavior.
