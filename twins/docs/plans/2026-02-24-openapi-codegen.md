# Phase 3: OpenAPI-Driven Code Generation

> Plan for generating the mechanical 42-50% of twin code from API specifications, based on patterns observed in two manually-built twins (Google Drive and Gmail).

## Context

After building two digital twins manually, we have concrete data on what code is domain-specific vs mechanical:

| Classification | Drive lines | Gmail lines | % of non-test |
|---|---:|---:|---:|
| **Domain-specific** (must be hand-written) | ~566 | ~1,148 | 55-65% |
| **Generatable** (from API spec) | ~607 | ~1,110 | 32-35% |
| **Templatable** (same pattern, thin customization) | ~283 | ~326 | 10-15% |

The generatable code follows predictable patterns:
- V1/V3 response types (Rust structs matching API JSON shapes)
- HTTP body types (request/response structs for route handlers)
- Route handler functions (extract params, call `handle()`, convert response)
- `routes()` wiring (list of method/path/handler tuples)
- Error response helpers

This plan describes a code generator that takes a twin specification and produces these mechanical artifacts, so developers only write domain-specific logic.

---

## Goals

1. Reduce new twin creation time by ~40% by generating mechanical code
2. Ensure generated code is human-readable and editable (not hidden behind macros)
3. Support incremental adoption — existing twins (Drive, Gmail) can optionally migrate
4. Keep the generator simple enough to maintain as a `twin-cli generate` subcommand
5. Produce code that compiles and passes `cargo check` without manual fixup

## Non-goals

- Full OpenAPI spec parsing (Google uses Discovery Documents, not standard OpenAPI)
- Generating domain logic (`handle()`, seeding, assertions)
- Replacing hand-written twins — generated code is a starting point, not a cage
- Supporting non-Google APIs in V1 (design for it, don't implement it)

---

## Design

### Input: Twin Spec File

Each twin is described by a TOML spec file (e.g., `specs/gmail.toml`). TOML chosen over YAML for Rust ecosystem alignment and over JSON for human authoring.

```toml
[twin]
name = "gmail"
api_version = "v1"
base_path = "/gmail/v1/users/me"
state_type = "GmailTwinService"
request_type = "GmailRequest"
response_type = "GmailResponse"

# --- Resources ---

[[resources]]
name = "Message"
plural = "messages"

[[resources.operations]]
name = "list"
method = "GET"
path = "/messages"
handler = "route_v1_list_messages"
request_variant = "ListMessages"
response_variant = "MessageList"
success_status = 200

[resources.operations.query_params]
labelIds = { type = "Option<String>", rename = "label_ids" }
maxResults = { type = "u32", default = "100" }
pageToken = { type = "Option<String>" }

[[resources.operations]]
name = "get"
method = "GET"
path = "/messages/{id}"
handler = "route_v1_get_message"
request_variant = "GetMessage"
response_variant = "Message"
success_status = 200

[resources.operations.path_params]
id = { type = "String", rename = "message_id" }

[resources.operations.query_params]
format = { type = "Option<String>", default = "\"full\"" }

[[resources.operations]]
name = "send"
method = "POST"
path = "/messages/send"
handler = "route_v1_send_message"
request_variant = "SendMessage"
response_variant = "Message"
success_status = 200
body_type = "V1SendBody"

[[resources.operations]]
name = "delete"
method = "DELETE"
path = "/messages/{id}"
handler = "route_v1_delete_message"
request_variant = "DeleteMessage"
response_variant = "Deleted"
success_status = 204

[resources.operations.path_params]
id = { type = "String", rename = "message_id" }

[[resources.operations]]
name = "modify"
method = "POST"
path = "/messages/{id}/modify"
handler = "route_v1_modify_message"
request_variant = "ModifyMessage"
response_variant = "Message"
success_status = 200
body_type = "V1ModifyBody"

[resources.operations.path_params]
id = { type = "String", rename = "message_id" }

# ... more operations ...

# --- Response Types ---
# These describe the API-facing JSON shapes (camelCase).
# The generator produces Rust structs with #[serde(rename_all = "camelCase")].

[[response_types]]
name = "V1MessageRef"
fields = [
    { name = "id", type = "String" },
    { name = "thread_id", type = "String" },
]

[[response_types]]
name = "V1MessageList"
fields = [
    { name = "messages", type = "Vec<V1MessageRef>" },
    { name = "next_page_token", type = "Option<String>", skip_none = true },
    { name = "result_size_estimate", type = "u32" },
]

# ... more types ...

# --- Request Body Types ---
# These describe HTTP request bodies (also camelCase).

[[body_types]]
name = "V1SendBody"
fields = [
    { name = "to", type = "Vec<String>" },
    { name = "cc", type = "Vec<String>", default = "Vec::new()" },
    { name = "bcc", type = "Vec<String>", default = "Vec::new()" },
    { name = "subject", type = "Option<String>" },
    { name = "body", type = "Option<String>" },
    { name = "thread_id", type = "Option<String>" },
]

[[body_types]]
name = "V1ModifyBody"
fields = [
    { name = "add_label_ids", type = "Vec<String>", default = "Vec::new()" },
    { name = "remove_label_ids", type = "Vec<String>", default = "Vec::new()" },
]

# --- Native Routes (optional, simpler pattern) ---

[[native_routes]]
name = "send_message"
method = "POST"
path = "/gmail/messages/send"
handler = "route_send_message"
request_variant = "SendMessage"
response_variant = "Message"
body_type = "NativeSendBody"
```

### Output: Generated Rust Code

The generator produces a single file (e.g., `crates/twin-gmail/src/generated.rs`) containing:

1. **Response type structs** — `#[derive(Serialize)]` with `#[serde(rename_all = "camelCase")]`
2. **Request body structs** — `#[derive(Deserialize)]` with `#[serde(rename_all = "camelCase")]`
3. **Query parameter structs** — `#[derive(Deserialize)]` per operation that has query params
4. **Route handler functions** — async functions matching the route handler pattern
5. **Routes wiring function** — `pub fn v1_routes(shared: SharedTwinState<T>) -> Router`
6. **Error response helpers** — `v1_error_response()`, `twin_error_to_v1_response()`

The generated code is checked into the repo (not generated at build time). This is intentional:
- Developers can read and understand it
- Custom modifications are preserved via a `// GENERATED — DO NOT EDIT ABOVE` / `// CUSTOM CODE BELOW` convention, or by keeping custom code in `lib.rs` and only generated code in `generated.rs`
- No build-time proc-macro complexity
- `cargo check` validates the output immediately

### What is NOT generated

The developer still writes in `lib.rs`:
- Domain model structs (entities, internal enums)
- `GmailRequest` / `GmailResponse` enums
- The `handle()` method (core business logic)
- `seed_from_scenario()`, `evaluate_assertion()`, `execute_timeline_action()`, `validate_scenario()`
- `StateInspectable` impl
- Response conversion functions (e.g., `gmail_message_to_v1()`) — these bridge domain types to API types and require domain knowledge
- Native route handlers (simpler, fewer, and use internal types)

### Module structure

```
crates/twin-gmail/
  src/
    lib.rs           # Domain model, handle(), TwinService impl, hand-written code
    generated.rs     # Generated response types, body types, route handlers, wiring
    # Developer imports generated.rs via: mod generated; pub use generated::*;
```

---

## Architecture

### Generator implementation

The generator is a new subcommand in `twin-cli`:

```
twin-cli generate --spec specs/gmail.toml --output crates/twin-gmail/src/generated.rs
```

It is a pure code generation tool:
1. Parse the TOML spec
2. Build an in-memory model of resources, operations, types
3. Emit Rust source code using string templates (not proc-macros, not syn/quote)

**Why string templates, not syn/quote?**
- The output is static Rust source checked into the repo
- String templates are simpler to write and debug than AST manipulation
- The generated code is simple enough (struct definitions, function signatures) that AST manipulation adds no value
- Developers can read the templates in the generator code and predict what they'll get

### Code generation templates

Each output section has a template. For example, the route handler template:

```rust
// Template for a standard route handler:
async fn {handler_name}(
    State(state): State<{state_type}>,
    resolved: Option<Extension<ResolvedActorId>>,
    headers: axum::http::HeaderMap,
    {path_param_extractor}
    {query_param_extractor}
    {body_extractor}
) -> impl IntoResponse {{
    let actor_id = extract_actor_id(&resolved, &headers);
    let mut rt = state.lock().await;
    let result = rt.service.handle({request_type}::{request_variant} {{
        actor_id,
        {field_mappings}
    }});
    match result {{
        Ok({response_type}::{response_variant}{response_destructure}) => {{
            {response_conversion}
            (StatusCode::{success_status}, Json(serde_json::to_value(&v1).unwrap())).into_response()
        }}
        Err(e) => twin_error_to_v1_response(e),
        _ => v1_error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected response"),
    }}
}}
```

### The response conversion problem

The hardest part of code generation is the response conversion — transforming a domain response variant into a V1 API JSON shape. This varies significantly:

| Operation | Drive | Gmail |
|---|---|---|
| Get file/message | `drive_item_to_v3_file(&item, true)` | `gmail_message_to_v1(&msg, format)` |
| List | Inline mapping to `V3FileList` | Inline mapping to `V1MessageList` |
| Delete | Empty 204 response | Empty 204 response |
| Modify | Same as Get | Same as Get |

**Decision:** The generator produces route handlers that call a user-provided conversion function. The spec declares which conversion function to call:

```toml
[[resources.operations]]
name = "get"
# ...
response_conversion = "gmail_message_to_v1(&msg, format)"
```

For simple cases (delete → 204, ok → 200 with no body), the generator handles it directly. For complex cases, it emits a call to a hand-written function.

---

## Phases

### Phase 3A: Spec format + struct generation

> Generate response types, body types, and query param structs from a TOML spec.

| # | Task | Est. lines |
|---|------|-----------|
| 1 | Define TOML spec schema (Rust structs for deserializing the spec) | ~80 |
| 2 | Add `twin-cli generate` subcommand with spec parsing | ~60 |
| 3 | Implement response type code generation (Serialize structs) | ~80 |
| 4 | Implement body type code generation (Deserialize structs) | ~60 |
| 5 | Implement query param struct generation | ~50 |
| 6 | Write a Gmail spec file (`specs/gmail.toml`) | ~200 |
| 7 | Verify generated structs match existing hand-written ones | ~40 |

**Validation:** Generate Gmail's response/body types, diff against hand-written versions, ensure they're equivalent.

### Phase 3B: Route handler generation

> Generate V1 route handler functions and routes() wiring.

| # | Task | Est. lines |
|---|------|-----------|
| 8 | Implement route handler code generation from operation specs | ~150 |
| 9 | Implement routes wiring function generation | ~40 |
| 10 | Implement error helper generation | ~30 |
| 11 | Handle edge cases: operations with no body, no query params, path-only, multi-path-param | ~60 |
| 12 | Write a Drive spec file (`specs/drive.toml`) | ~150 |
| 13 | Generate for both Gmail and Drive, verify output compiles | ~40 |

**Validation:** Generate both twins' route handlers, ensure `cargo check` passes.

### Phase 3C: Integration + migration

> Wire generated code into existing twins, verify all tests pass.

| # | Task | Est. lines |
|---|------|-----------|
| 14 | Create `generated.rs` for Gmail, wire into lib.rs with `mod generated` | ~20 |
| 15 | Remove hand-written equivalents from Gmail's lib.rs that are now generated | ~0 (deletions) |
| 16 | Do the same for Drive | ~20 |
| 17 | Run full test suite, fix any mismatches | ~50 |
| 18 | Add `twin-cli generate` tests | ~100 |

**Validation:** All 230 tests pass after migration.

### Phase 3D: Polish + documentation

| # | Task | Est. lines |
|---|------|-----------|
| 19 | Handle `#[serde(skip_serializing_if)]` for optional fields | ~20 |
| 20 | Support custom `#[serde]` attributes in spec (e.g., flatten, default) | ~30 |
| 21 | Add `--check` mode to verify generated code is up-to-date (for CI) | ~40 |
| 22 | Update scaffolding templates to include `generated.rs` and spec file | ~30 |

---

## Estimated Totals

| Category | Lines |
|----------|------:|
| Spec schema (Rust types for parsing TOML) | ~80 |
| CLI subcommand + arg parsing | ~60 |
| Code generation engine | ~420 |
| Spec files (Gmail + Drive) | ~350 |
| Tests | ~140 |
| Migration/wiring | ~40 |
| **Total new code** | **~1,090** |
| **Code removed** (from Gmail + Drive) | **~1,200** |
| **Net change** | **~-110** |

The generator pays for itself: ~1,090 lines of generator code replaces ~1,200 lines across two twins, and every future twin saves 500-1,100 lines.

---

## Detailed Design Decisions

### Why TOML spec files (not OpenAPI directly)?

1. **Google APIs use Discovery Documents**, not OpenAPI. Converting Discovery → OpenAPI → our format is two lossy translations.
2. **We implement subsets** of APIs. An OpenAPI spec describes the full API; we need to describe which operations our twin supports and how they map to internal domain types.
3. **The spec contains twin-specific information** that doesn't exist in any API spec: domain request variant names, response conversion function names, field remappings.
4. **TOML is simple** to write by hand. A developer authoring a new twin describes each endpoint in ~5-10 lines.
5. **Future:** If we want OpenAPI import, it can be a separate tool that generates TOML spec files, which are then reviewed and hand-tuned.

### Why generate checked-in code (not build-time)?

1. **Readability** — developers can read `generated.rs` to understand what routes exist
2. **Debuggability** — stack traces point to real line numbers in real files
3. **Editability** — if a route needs special handling, the developer modifies it directly
4. **No build dependency** — `cargo build` doesn't need the generator; only `twin-cli generate` does
5. **Diffable** — PRs show exactly what changed in generated code

### Why not a proc-macro?

1. Proc-macros hide generated code behind `#[derive(...)]` — you can't see or debug it easily
2. Proc-macros increase compile times for every build
3. The `#[derive(TwinSnapshot)]` macro is appropriate because snapshot/restore is a small, well-defined transformation. Route handler generation is too large and varied for a derive.
4. We already have `twin-macros` for the right-sized derive use cases

### Error response helpers — move to framework?

Both twins have nearly identical error helpers:

```rust
fn v1_error_response(status: StatusCode, message: &str) -> Response { ... }
fn twin_error_to_v1_response(err: TwinError) -> Response { ... }
```

**Decision:** Keep them generated per twin for now. The status code mapping and error format differ between Google API versions (v1 vs v3 use slightly different error JSON shapes). If a third twin uses the exact same format, consider extracting to `twin-server-core`.

### Actor ID extraction — custom extractor?

Every V1 route handler starts with:
```rust
let actor_id = extract_actor_id(&resolved, &headers);
```

**Decision:** Keep as a function call in generated code. A custom axum extractor would be cleaner but adds framework complexity. The function call is 1 line per handler — not worth the abstraction yet.

---

## Spec Format Reference

### Full field reference for `[[resources.operations]]`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | String | Yes | Operation name (e.g., "list", "get", "create") |
| `method` | String | Yes | HTTP method (GET, POST, PUT, PATCH, DELETE) |
| `path` | String | Yes | URL path relative to `base_path` |
| `handler` | String | Yes | Generated function name |
| `request_variant` | String | Yes | `GmailRequest::Variant` name |
| `response_variant` | String | Yes | `GmailResponse::Variant` name |
| `success_status` | Integer | Yes | HTTP status code on success |
| `body_type` | String | No | Name of request body struct |
| `response_conversion` | String | No | Expression to convert domain response to V1 type |
| `path_params` | Table | No | Path parameter definitions |
| `query_params` | Table | No | Query parameter definitions |
| `field_mappings` | Table | No | Custom mappings from HTTP inputs to request variant fields |

### Full field reference for `[[response_types]]` / `[[body_types]]`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | String | Yes | Struct name |
| `fields[].name` | String | Yes | Field name (snake_case in Rust, camelCase in JSON via serde) |
| `fields[].type` | String | Yes | Rust type |
| `fields[].skip_none` | Boolean | No | Add `#[serde(skip_serializing_if = "Option::is_none")]` |
| `fields[].default` | String | No | Default value expression |
| `fields[].flatten` | Boolean | No | Add `#[serde(flatten)]` |

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Generated code doesn't match hand-written patterns exactly | Medium | Medium | Start with Gmail, diff against existing code, iterate on templates |
| TOML spec becomes unwieldy for complex APIs | Low | Medium | Keep spec focused on mechanical parts; complex logic stays hand-written |
| Route handlers need more variation than templates support | Medium | Low | Add `response_conversion` escape hatch for custom logic; allow overriding generated handlers |
| Migration breaks existing tests | Low | High | Run full test suite after each migration step; keep hand-written code as reference |
| String-template codegen produces formatting issues | Low | Low | Run `rustfmt` on generated output |
| Generator maintenance burden | Low | Medium | Generator is ~500 lines of straightforward template expansion; low ongoing cost |

---

## Success Criteria

- `twin-cli generate --spec specs/gmail.toml` produces code that compiles
- Generated Gmail code is functionally equivalent to the current hand-written implementation
- Generated Drive code is functionally equivalent to the current hand-written implementation
- All 230 existing tests pass after migration
- A new twin can be started with `twin-cli new foo` + `twin-cli generate --spec specs/foo.toml`
- Developer only needs to write: domain model, handle(), seed/assertion/timeline logic, conversion functions
- Generated code is readable, formatted (via rustfmt), and has doc comments

---

## Dependencies

| Dependency | Purpose | Already in workspace? |
|---|---|---|
| `toml` | Parse spec files | No — add to twin-cli |
| `rustfmt` (external) | Format generated code | System tool, not a Rust dep |

No other new dependencies needed. The generated code uses the same deps as existing twins (axum, serde, serde_json).

---

## Future Considerations (not in scope)

| Idea | When to revisit |
|---|---|
| **OpenAPI/Discovery Document import** → generate TOML specs from API docs | After 3+ twins built, if manually writing specs feels slow |
| **Diff tool** — detect changes between spec versions and show what code needs regenerating | After codegen is stable and twins are being actively maintained |
| **Test generation** — generate basic route handler tests from specs | After Phase 3C, if test boilerplate is identified |
| **Multi-file generation** — split generated code into routes.rs, types.rs, etc. | If generated.rs exceeds ~1,000 lines |
