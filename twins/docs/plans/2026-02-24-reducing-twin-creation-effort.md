# Reducing Twin Creation Effort

> Plan for making it significantly easier and faster to create new digital twins. Based on a detailed audit of the twin-drive implementation (~1,940 lines of non-test code) which found that ~700 lines (36%) are mechanical boilerplate or mapping code that could be generated or defaulted.

## Context

Creating a new twin today requires implementing the full `TwinService` trait (8 methods), writing HTTP route handlers, response type mappings, state inspection endpoints, snapshot/restore logic, scenario seeding, and route wiring. The twin-drive reference implementation has ~1,940 lines of non-test code. Of that:

| Category | Lines | Classification |
|----------|------:|---------------|
| Domain model (entities, enums) | ~95 | DOMAIN-SPECIFIC |
| Request/Response enums | ~65 | DOMAIN-SPECIFIC |
| Core business logic (`handle()`) | ~320 | DOMAIN-SPECIFIC |
| Helper methods (ID gen, permissions) | ~115 | DOMAIN-SPECIFIC |
| V3 response types + mapping | ~115 | MAPPING (generatable) |
| V3 HTTP handlers (route functions) | ~316 | MAPPING (generatable) |
| Native HTTP handlers | ~122 | BOILERPLATE |
| HTTP body types (request/response structs) | ~94 | MAPPING (generatable) |
| State inspection | ~128 | BOILERPLATE |
| Snapshot/restore | ~60 | BOILERPLATE |
| Scenario seeding | ~70 | BOILERPLATE |
| Assertions + validation | ~80 | MIXED |
| Timeline action dispatch | ~60 | BOILERPLATE |
| `routes()` wiring | ~35 | BOILERPLATE |

**Goal:** Reduce the effort to create a new twin so that a developer only writes domain-specific logic (~1,240 lines for a Drive-complexity twin) and the framework handles the rest.

---

## Ideas Evaluated

| # | Idea | Leverage | Complexity | Verdict |
|---|------|----------|-----------|---------|
| 1 | **Trait default methods** | Low-medium (~90 lines) | Low | DO FIRST |
| 2 | **Scaffolding CLI fixes** | Low (quality-of-life) | Low | DO SECOND |
| 3 | **State inspection in framework** | Medium (~130 lines) | Medium | DO THIRD |
| 4 | **Derive macros** | Medium (~190 lines) | Medium-high | DO FOURTH |
| 5 | **OpenAPI-driven codegen** | High (~525 lines) | High | DEFER to Phase 3 |
| 6 | **Twin spec DSL** | High | Very high | SKIP (premature) |
| 7 | **OpenAPI spec diffing** | Low | High | SKIP (premature) |

### Rationale for ordering

- **Trait defaults first** because they're the lowest-risk, highest-certainty improvement. Two methods (`reset`, `validate_scenario`) are trivially defaultable today. Two more (`service_snapshot`, `service_restore`) can be defaulted with `Serialize`/`Deserialize` bounds. This eliminates boilerplate from every future twin with zero new infrastructure.

- **Scaffolding fixes second** because the CLI is the entry point for new twin authors and it currently has a known bug (hyphenated names produce invalid Rust identifiers) and a naive template engine. Fixing these is low-effort and high-impact on first impressions.

- **State inspection third** because every twin needs it, the pattern is identical across twins (iterate items, return a tree), and moving it to the framework means twin authors don't need to write ~130 lines of state endpoint boilerplate.

- **Derive macros fourth** because they require proc-macro crate infrastructure but unlock powerful per-struct code generation (snapshot serialization with custom encoding, state inspection formatting, route wiring).

- **OpenAPI codegen deferred** because it's the highest-leverage item (~525 lines per twin) but also the highest complexity. It requires: parsing OpenAPI specs, generating Rust code, handling Google's Discovery Document format vs standard OpenAPI, dealing with partial API coverage, and maintaining the generator. Better to do this after the simpler improvements are in place and we have a second twin to validate patterns against.

- **Twin spec DSL and spec diffing skipped** because they're premature abstractions. We have one twin. We need at least 2-3 before we know what the right DSL shape is.

---

## Phase 2A: Trait Defaults (4 tasks)

> Reduce `TwinService` from 8 required methods to 4-5 required methods.

### Task 1: Add default `reset()` implementation

**File:** `crates/twin-service/src/lib.rs:124`

Add a `Default` bound to `TwinService` and provide a default `reset()`:

```rust
pub trait TwinService: Sized + Send + Sync + Default + 'static {
    // ...

    /// Reset to default empty state.
    fn reset(&mut self) {
        *self = Self::default();
    }
}
```

Every twin already implements `Default` (required by `TwinRuntime::reset()`). The twin-drive implementation already does exactly `*self = Self::default()` in its `reset()`. This just makes it the default so new twins get it for free.

**Update twin-drive:** Remove the explicit `reset()` implementation since it matches the default.

---

### Task 2: Add default `validate_scenario()` implementation

**File:** `crates/twin-service/src/lib.rs:121`

Provide a default that returns no errors and no warnings:

```rust
fn validate_scenario(_scenario: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    (Vec::new(), Vec::new())
}
```

This matches what most new twins will start with (no domain-specific validation). Twins can override when they add validation logic.

**Update twin-drive:** Keep its override (it has actual validation logic), but verify the test suite still passes.

---

### Task 3: Add default `service_snapshot()` with `Serialize` bound

**File:** `crates/twin-service/src/lib.rs:89`

Add a default implementation gated on `Self: Serialize`:

```rust
fn service_snapshot(&self) -> serde_json::Value
where
    Self: serde::Serialize,
{
    serde_json::to_value(self).expect("TwinService snapshot serialization failed")
}
```

**Problem:** Rust doesn't support `where Self: Serialize` on trait methods with defaults when the trait itself doesn't require `Serialize`. There are two approaches:

**Option A — Add `Serialize` bound to trait:**
```rust
pub trait TwinService: Sized + Send + Sync + Default + Serialize + 'static { ... }
```
This forces all twins to derive `Serialize`, which is reasonable since they all need snapshot support anyway.

**Option B — Keep it manual, document the pattern:**
Leave `service_snapshot()` as required but add a one-liner recipe in docs that works for most twins.

**Recommendation:** Option A. The twin-drive implementation can't use the default (it does custom base64 encoding for binary content), but it can still override. Future simple twins (Slack, GitHub) where all state is JSON-serializable will benefit from the default.

**Caveat:** twin-drive stores `BTreeMap<ItemId, Vec<u8>>` in its content store. `Vec<u8>` serializes as a JSON array of numbers by default, not base64. twin-drive overrides to use base64 encoding. The default `serde_json::to_value(self)` would work but produce larger snapshots. twin-drive should continue to override.

---

### Task 4: Add default `service_restore()` with `Deserialize` bound

**File:** `crates/twin-service/src/lib.rs:92`

Paired with Task 3. If we go with Option A (adding `Serialize + Deserialize` bounds):

```rust
fn service_restore(&mut self, snapshot: &serde_json::Value) -> Result<(), TwinError> {
    *self = serde_json::from_value(snapshot.clone())
        .map_err(|e| TwinError::Operation(format!("snapshot restore failed: {e}")))?;
    Ok(())
}
```

**Same caveat:** twin-drive overrides this to handle base64 decoding.

---

### Task 4b: Update twin-drive to use defaults where applicable

- Remove `reset()` impl (uses default)
- Keep `validate_scenario()` override (has real logic)
- Keep `service_snapshot()` override (base64 encoding)
- Keep `service_restore()` override (base64 decoding)
- Add `Serialize, Deserialize` derives to `DriveTwinService` if not already present
- Run full test suite: `cargo test`

---

## Phase 2B: Scaffolding CLI Fixes (3 tasks)

> Fix bugs and improve the template engine so generated twins compile and are more useful.

### Task 5: Fix hyphenated name bug in templates

**File:** `apps/twin-cli/src/main.rs`

The current template engine does two replacements:
- `{{name}}` → lowercase name (e.g., `my-slack`)
- `{{Name}}` → PascalCase (e.g., `MySlack`)

Problem: Rust module paths use underscores, not hyphens. `use twin_my-slack` is invalid.

**Fix:** Add a third replacement:
- `{{name_snake}}` → snake_case (e.g., `my_slack`)

Update templates to use `{{name_snake}}` in `use` statements and module paths:

```rust
// In templates/twin-crate/lib.rs.tmpl — no change needed (it's the crate itself)
// In templates/twin-server/main.rs.tmpl:
use twin_{{name_snake}}::{{Name}}TwinService;
```

Also update `Cargo.toml.tmpl` files to use the correct crate name format.

---

### Task 6: Generate working `reset()` and `validate_scenario()` from template

**File:** `templates/twin-crate/lib.rs.tmpl`

With trait defaults from Phase 2A, the generated twin no longer needs `todo!()` stubs for `reset()` and `validate_scenario()`. Remove those method impls from the template.

For `service_snapshot()` and `service_restore()`, if we added `Serialize + Deserialize` bounds (Task 3 Option A), remove those stubs too — the defaults work for a fresh twin with no custom encoding needs.

The template should generate a twin with only 4 required methods, all with `todo!()`:
- `routes()`
- `seed_from_scenario()`
- `evaluate_assertion()`
- `execute_timeline_action()`

And a minimal compilable struct:

```rust
#[derive(Default, Serialize, Deserialize)]
pub struct {{Name}}TwinService {
    // Add your domain state here
}
```

---

### Task 7: Add basic route skeleton to generated twin

**File:** `templates/twin-crate/lib.rs.tmpl`

Instead of a `todo!()` for `routes()`, generate a minimal working router with:
- A health check route (`GET /` returning 200)
- A placeholder native route (commented out example)

This means `twin-cli new my-thing && cargo build` produces a binary that starts and responds to requests, even before any domain logic is written.

---

## Phase 2C: State Inspection in Framework (3 tasks)

> Move the generic state inspection pattern out of individual twins and into twin-server-core.

### Task 8: Define `StateInspectable` trait in twin-service

**File:** `crates/twin-service/src/lib.rs` (new trait)

```rust
/// A tree node for state inspection responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateNode {
    pub id: String,
    pub label: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, serde_json::Value>,
}

/// Trait for twins that support state inspection.
pub trait StateInspectable {
    /// Return all state nodes for the tree view.
    fn inspect_state(&self) -> Vec<StateNode>;

    /// Return a single node by ID, or None.
    fn inspect_node(&self, id: &str) -> Option<StateNode>;
}
```

This is opt-in (not part of `TwinService`). Twins that implement it get automatic state inspection routes.

---

### Task 9: Add generic state inspection routes in twin-server-core

**File:** `crates/twin-server-core/src/lib.rs`

When building the router, check if `T: StateInspectable` and conditionally add:
- `GET /state/items` — returns all nodes
- `GET /state/items/:id` — returns a single node
- `GET /state/tree` — returns nodes organized as a tree (computed from parent_id)

This uses Rust's conditional trait bounds:

```rust
pub fn build_router<T>(shared: SharedTwinState<T>) -> Router
where
    T: TwinService + StateInspectable,
{
    // ... existing routes ...
    // ... add state inspection routes ...
}
```

**Alternative:** If we don't want to require `StateInspectable` on all twins, use a separate `with_state_inspection()` builder method or a feature flag.

**Recommendation:** Make it a separate extension method so twins opt in:

```rust
// In the twin's routes() method:
fn routes(shared: SharedTwinState<Self>) -> Router {
    let mut router = Router::new();
    // ... domain routes ...
    router = router.merge(state_inspection_routes(shared.clone()));
    router
}
```

---

### Task 10: Migrate twin-drive to use `StateInspectable`

**File:** `crates/twin-drive/src/lib.rs`

Replace the ~128 lines of hand-written state inspection endpoints with a `StateInspectable` impl:

```rust
impl StateInspectable for DriveTwinService {
    fn inspect_state(&self) -> Vec<StateNode> {
        self.items.values().map(|item| StateNode {
            id: item.id.clone(),
            label: item.name.clone(),
            kind: match item.kind {
                DriveItemKind::File => "file".into(),
                DriveItemKind::Folder => "folder".into(),
            },
            parent_id: item.parent_id.clone(),
            properties: /* mime_type, size, owner, permissions, etc. */,
        }).collect()
    }

    fn inspect_node(&self, id: &str) -> Option<StateNode> {
        self.items.get(id).map(/* same mapping */)
    }
}
```

Remove the old hand-written `/state/*` route handlers and their body types.

**Verify:** All existing state inspection tests still pass (response shape may change — update test assertions if needed).

---

## Phase 2D: Derive Macros (4 tasks)

> Create a proc-macro crate for code generation via `#[derive(...)]` attributes.

### Task 11: Create `twin-macros` proc-macro crate

**Files:** `crates/twin-macros/Cargo.toml`, `crates/twin-macros/src/lib.rs`

Set up a new proc-macro crate in the workspace:

```toml
[package]
name = "twin-macros"
version = "0.1.0"
edition = "2024"

[lib]
proc-macro = true

[dependencies]
syn = { version = "2", features = ["full"] }
quote = "1"
proc-macro2 = "1"
```

Add to workspace `Cargo.toml` members.

---

### Task 12: Implement `#[derive(TwinSnapshot)]`

**File:** `crates/twin-macros/src/lib.rs`

Generates `service_snapshot()` and `service_restore()` implementations. By default, uses `serde_json::to_value` / `from_value`. Supports field-level attributes for custom encoding:

```rust
#[derive(TwinSnapshot)]
pub struct DriveTwinService {
    items: BTreeMap<ItemId, DriveItem>,
    #[twin_snapshot(encode = "base64")]
    content: BTreeMap<ItemId, Vec<u8>>,
    next_id: u64,
}
```

The `#[twin_snapshot(encode = "base64")]` attribute tells the macro to base64-encode that field in the snapshot and decode on restore.

**This replaces ~60 lines** of manual snapshot/restore code per twin.

---

### Task 13: Implement `#[derive(StateInspectable)]` (optional)

**File:** `crates/twin-macros/src/lib.rs`

If the `StateInspectable` trait from Phase 2C proves to have a repetitive impl pattern across twins, add a derive macro. This is speculative — we may find that each twin's `inspect_state()` is different enough that a derive doesn't help. **Evaluate after building a second twin.**

**Status: TENTATIVE** — implement only if the pattern is clear after Phase 2C.

---

### Task 14: Migrate twin-drive to use `#[derive(TwinSnapshot)]`

**File:** `crates/twin-drive/src/lib.rs`

Replace the hand-written `service_snapshot()` and `service_restore()` methods with the derive macro. Verify snapshot format is backward-compatible (base64 encoding for content blobs must produce the same JSON shape).

---

## What We Skip (and Why)

### OpenAPI-driven codegen — DEFERRED to Phase 3

This is the highest-leverage idea (~525 lines per twin) but premature for three reasons:

1. **We only have one twin.** The codegen templates would be shaped entirely around Google Drive's API. When we build a Slack or GitHub twin, we'll discover different patterns (webhooks, pagination styles, auth models) that would reshape the generator.

2. **Google uses Discovery Documents, not OpenAPI.** We'd need to either parse their proprietary format or use a third-party conversion. Both add complexity and maintenance burden.

3. **The simpler improvements come first.** Trait defaults + derive macros + state inspection will reduce the ~700 lines of boilerplate to ~300-400 lines. OpenAPI codegen can then target just the remaining mapping code (response types, HTTP handler wiring).

**Revisit after:** A second twin (Slack or GitHub) is built manually, validating which patterns are truly universal.

### Twin spec DSL — SKIP

A declarative YAML/TOML DSL for defining twins is a big abstraction. We don't know the right shape yet. Building it now would either be too rigid (can't express real twin complexity) or too flexible (just another programming language). Better to let patterns emerge from 2-3 manually-built twins.

### OpenAPI spec diffing — SKIP

Automatically detecting API changes and updating twins is a maintenance tool, not a creation tool. It's only useful after we have codegen and multiple twins. Premature.

---

## Implementation Order

```
Phase 2A: Trait Defaults          (Tasks 1-4b)   ✅ COMPLETE
Phase 2B: Scaffolding CLI Fixes   (Tasks 5-7)    ✅ COMPLETE
Phase 2C: State Inspection        (Tasks 8-10)   ✅ COMPLETE
Phase 2D: Derive Macros           (Tasks 11-14)  ✅ COMPLETE
```

Total: ~5-8 sessions. After this, creating a new twin requires writing only:
- Domain model structs
- Core business logic (`handle()` equivalent)
- `seed_from_scenario()` — domain-specific JSON interpretation
- `evaluate_assertion()` — domain-specific assertion logic
- `execute_timeline_action()` — action dispatch
- `routes()` — domain-specific HTTP endpoints (but with framework-provided state inspection)

The mechanical parts (snapshot/restore, reset, validation stubs, state inspection, route wiring boilerplate) are handled by the framework.

---

## Success Criteria

- A new twin generated by `twin-cli new my-thing` compiles and runs immediately
- The `TwinService` trait requires implementing only 4-5 methods (down from 8)
- State inspection is provided by the framework, not hand-written per twin
- twin-drive uses derive macros for snapshot/restore
- All 183+ existing tests continue to pass after each phase
- The scaffolding CLI handles hyphenated names correctly

---

## Dependencies

| Phase | New crates | New dependencies |
|-------|-----------|-----------------|
| 2A | None | None |
| 2B | None | None |
| 2C | None | None |
| 2D | `twin-macros` | `syn 2`, `quote 1`, `proc-macro2 1` |

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Adding `Serialize + Deserialize` bounds to `TwinService` breaks twins with non-serializable state | Low | Medium | All twins should have serializable state for snapshots anyway |
| Derive macro for snapshots produces different JSON shape than hand-written code | Medium | High | Test backward compatibility explicitly; keep base64 encoding identical |
| `StateInspectable` trait is too rigid for some twins | Low | Low | It's opt-in, twins can always hand-write state routes |
| Proc-macro compilation slows down builds | Low | Low | Proc-macro crate is small, compiles once |
