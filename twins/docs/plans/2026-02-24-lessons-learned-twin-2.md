# Lessons Learned: Building Twin #2 (Gmail)

> What we learned from building the Gmail digital twin — the second twin after Google Drive — and how it informs the framework and future codegen.

## Summary

Building a second twin validated the framework design and produced concrete data for code generation. The Gmail twin took ~4,311 lines (vs Drive's ~4,180), confirming that twins of similar API surface area produce similar-sized implementations. The framework required one change (opaque scenario actions/assertions), and all other abstractions worked as designed.

---

## 1. Framework Validation Results

### What worked perfectly

| Framework feature | Validation |
|---|---|
| `TwinService` trait (4 required + 4 defaulted) | Gmail implemented all 4 required methods with no friction. Default `reset()`, `validate_scenario()`, `service_snapshot()`, `service_restore()` all worked as-is. |
| `#[derive(TwinSnapshot)]` | Generated correct snapshot/restore code, including `#[twin_snapshot(encode = "base64")]` for Gmail's attachment blobs. |
| `StateInspectable` + `state_inspection_routes()` | Gmail's thread→message hierarchy mapped cleanly to the `StateNode` tree model. |
| `twin-server-core` (auth, sessions, events, fault injection) | Zero changes needed. Auth middleware resolved actor IDs correctly. Sessions, event logging, and fault injection all worked for Gmail routes. |
| `twin-cli new gmail` | Scaffolded a compilable starting point that was iteratively filled in. |
| Scenario system (seed, assertions, timeline) | Worked for a completely different domain (mailbox vs file tree). |

### What needed fixing

| Issue | Fix | Impact |
|---|---|---|
| `Action` and `AssertionCheck` in `twin-scenario` were typed enums with only Drive-specific variants | Made both fields opaque (`serde_json::Value`) | Low — one-time change, backward compatible. Each twin now defines its own action/assertion schemas and deserializes them in `execute_timeline_action()` / `evaluate_assertion()`. |

### What was slightly awkward (but not worth changing)

| Observation | Detail |
|---|---|
| `routes()` is large | Gmail's `routes()` is 107 lines of pure wiring — every route must be manually listed with its handler, method, and path. This is correct (each twin controls its own route tree) but tedious. |
| V1 response mapping is verbose | Synthesizing Google's nested `payload.parts` structure from Gmail's flat internal model took ~358 lines. This is domain-specific but follows a predictable pattern per resource type. |
| Actor ID extraction boilerplate | Every V1 route handler starts with the same 5-line pattern: extract Extension, fall back to header, return 401 if missing. Could be a middleware or extractor, but it works fine as-is. |

---

## 2. Structural Comparison: Drive vs Gmail

### Line count breakdown

| Category | Drive | Gmail | Ratio | Classification |
|---|---:|---:|---|---|
| Domain model structs/enums | 98 | 97 | 1:1 | **Domain-specific** |
| Request/Response enums | (in above) | 160 | — | **Domain-specific** |
| Service struct + Default | 34 | 56 | 1:1.6 | **Domain-specific** |
| Helper methods | 115 | 190 | 1:1.7 | **Domain-specific** |
| Core business logic (handle) | 319 | 645 | 1:2.0 | **Domain-specific** |
| StateInspectable impl | 157 | 176 | 1:1.1 | **Templatable** — structure is identical |
| V1/V3 response types + mapping | 189 | 358 | 1:1.9 | **Generatable** — predictable per resource |
| TwinService impl (routes, seed, assertions, timeline, validate) | 410 | 557 | 1:1.4 | **Mixed** — routes are boilerplate, rest is domain |
| HTTP body types | 94 | 136 | 1:1.4 | **Generatable** — struct per endpoint |
| Native route handlers | 126 | 150 | 1:1.2 | **Templatable** — same pattern |
| V1/V3 mimicry route handlers | 324 | 616 | 1:1.9 | **Generatable** — predictable per endpoint |
| Unit tests (lib.rs) | 2,263 | 1,134 | 2:1 | **Domain-specific** |
| **Total** | **4,180** | **4,311** | | |

### Non-test code breakdown

| Classification | Drive lines | Gmail lines | What it means |
|---|---:|---:|---|
| **Domain-specific** (must be hand-written) | ~566 | ~1,148 | Domain model, handle(), helpers |
| **Generatable** (from API spec) | ~607 | ~1,110 | Response types, mapping, route handlers, body types |
| **Templatable** (same pattern, thin customization) | ~283 | ~326 | StateInspectable, native routes, routes() wiring |
| **Framework-provided** (already automatic) | — | — | snapshot/restore, reset, state inspection routes |

### Key ratios

- **Generatable code** is 32-35% of non-test lines in both twins
- **Templatable code** is 10-15% of non-test lines
- Combined: **42-50%** of twin code follows predictable patterns that a code generator could produce

---

## 3. Pattern Analysis for Codegen

### Patterns that are identical across twins

**1. Server binary** (~36 lines each)
```
EnvConfig → tracing → build_twin_router::<T>(config) → bind → serve
```
Already templated by `twin-cli`. No codegen value — it's a one-time scaffold.

**2. V1/V3 route handler shape**
Every mimicry route handler follows this exact pattern:
```rust
async fn route_v1_operation(
    State(shared): State<SharedTwinState<MyTwinService>>,
    Extension(actor_ext): Extension<ResolvedActorId>,  // or header fallback
    Path(id): Path<String>,                             // if path params
    Query(params): Query<MyQueryParams>,                // if query params
    Json(body): Json<MyBodyType>,                       // if request body
) -> impl IntoResponse {
    let actor_id = extract_actor_id_from_ext(&actor_ext);
    let mut state = shared.state.write().await;
    let request = MyRequest::Operation { actor_id, id, /* fields from body/query */ };
    match state.twin.handle(request) {
        Ok(MyResponse::VariantName(data)) => {
            let v1 = domain_to_v1_response(data);
            (StatusCode::OK, Json(v1)).into_response()
        }
        Err(e) => twin_error_to_v1_response(e),
    }
}
```
This is highly mechanical. A codegen input spec would need:
- HTTP method, path, path parameter names
- Query parameter struct (optional)
- Request body struct (optional)
- Domain request variant to construct
- Field mapping from HTTP inputs to domain request fields
- Expected response variant
- V1 response conversion function name
- Success status code

**3. Native route handler shape**
Same as V1 but simpler (no actor auth, simpler response types). Even more templatable.

**4. HTTP body types**
```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]  // V1 only
pub struct V1OperationBody {
    pub field_a: String,
    pub field_b: Option<Vec<String>>,
}
```
Directly derivable from an API spec's request/response schemas.

**5. V1/V3 response types**
```rust
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Resource {
    pub id: String,
    pub field_a: String,
    // ...
}
```
Also directly derivable from API spec response schemas.

**6. Error response helpers**
`v1_error_response()` and `twin_error_to_v1_response()` are nearly identical between Drive and Gmail — only the error domain strings differ. Could be a framework-provided generic.

### Patterns that vary by domain (NOT generatable)

| Pattern | Why it's domain-specific |
|---|---|
| `handle()` business logic | Core domain rules — permissions, threading, label semantics |
| `seed_from_scenario()` | Each twin has different dependency ordering (files need parent folders first; Gmail messages need labels first) |
| `evaluate_assertion()` | Each twin defines its own assertion types |
| `execute_timeline_action()` | Action types are domain-specific |
| `validate_scenario()` | Validation rules depend on domain constraints |
| Internal domain model | Entities, relationships, and state shape are unique per API |
| Payload synthesis (e.g., Gmail's `payload.parts`) | API-specific response structures that require domain knowledge to construct |

---

## 4. Estimation Accuracy

The Gmail plan estimated ~3,070 lines total. Actual was ~4,311 (40% over).

| Category | Estimated | Actual | % Over |
|---|---:|---:|---|
| Non-test code | 2,070 | 3,177 | +53% |
| Tests | 1,000 | 1,134 | +13% |

**Root causes of underestimation:**
1. **V1 response mapping** (+178 lines over estimate) — synthesizing `payload.parts` from flat fields was more complex than anticipated. Each message format (full, metadata, minimal) needed distinct rendering logic.
2. **Route handlers** (+156 lines over estimate) — auth extraction boilerplate repeated in every handler. More endpoints than initially scoped.
3. **StateInspectable** (+116 lines over estimate) — thread→message hierarchy needed more logic than the simple flat-item mapping in Drive.
4. **Helper methods** (+110 lines over estimate) — pagination helpers, snippet generation, date formatting.

**What was accurately estimated:**
- Domain model structs (within 3 lines)
- Unit test volume (within 13%)
- Native route handlers (within 25%)

**Lesson for future estimates:** API mimicry code (V1 response types, mapping functions, route handlers) is the hardest to estimate because it depends on the target API's response shape complexity. For future twins, estimate 2x what seems reasonable for mimicry code.

---

## 5. Decisions That Paid Off

1. **Simplified MIME model** — Storing messages as flat fields (from, to, subject, body_text, body_html) instead of recursive MIME trees was the right call. Agents work with structured fields, not raw RFC 2822. The `payload.parts` synthesis in V1 responses handles API compatibility.

2. **Subject-based threading** — Matching threads by subject prefix instead of Message-ID/In-Reply-To headers was sufficient for testing. Real Gmail uses header-based threading, but for agent scenarios where we control the seed data, subject matching works.

3. **Including attachments from day one** — Using `#[twin_snapshot(encode = "base64")]` for the attachment blob store worked exactly as it did for Drive's file content. No surprises.

4. **Reusing Drive's internal enum pattern** — The `GmailRequest`/`GmailResponse` enum pattern kept HTTP concerns out of `handle()`, making the business logic testable without any HTTP infrastructure.

5. **Making scenario actions/assertions opaque early** — Discovered during Gmail implementation that the typed enums in `twin-scenario` blocked new twins. Fixing it to `serde_json::Value` was a clean solution that scales to any number of twins.

---

## 6. Implications for Phase 3 (OpenAPI Codegen)

### What codegen should target (highest ROI)

| Target | Lines saved per twin | Difficulty |
|---|---:|---|
| V1/V3 response types | 100-360 | Low — direct schema mapping |
| HTTP body types (request/response structs) | 94-136 | Low — direct schema mapping |
| V1/V3 route handler stubs | 324-616 | Medium — needs operation→handler template |
| `routes()` wiring | 37-107 | Low — list of (method, path, handler) |
| Error response helpers | 20-30 | Low — generic, could be framework-provided |
| **Total** | **~575-1,250** | |

### What codegen should NOT target

| Target | Reason |
|---|---|
| Domain model (entities) | Too specific — each API has unique entity shapes and relationships |
| `handle()` business logic | Core domain rules can't be derived from an API spec |
| Scenario support (seed, assertions, timeline) | Domain-specific interpretation of scenarios |
| StateInspectable impl | Requires understanding of domain entity relationships (though it's templatable) |
| Native route handlers | Simple enough to hand-write, and they use the internal API (not the V1 shape) |

### Input spec shape

For codegen to work, it needs an input spec per twin that describes:

```yaml
twin:
  name: gmail
  api_version: v1
  base_path: /gmail/v1/users/me

resources:
  - name: Message
    operations:
      - name: list
        method: GET
        path: /messages
        query_params: { labelIds: "Vec<String>", maxResults: "u32", pageToken: "Option<String>" }
        response_type: MessageList
      - name: get
        method: GET
        path: /messages/{id}
        query_params: { format: "Option<String>" }
        response_type: Message
      - name: send
        method: POST
        path: /messages/send
        body_type: SendMessageBody
        response_type: Message
      # ...

response_types:
  Message:
    fields:
      id: String
      threadId: String
      labelIds: Vec<String>
      snippet: String
      # ...
  MessageList:
    fields:
      messages: Vec<MessageRef>
      nextPageToken: Option<String>
      resultSizeEstimate: u32
```

This spec could be derived from OpenAPI/Discovery Documents, then hand-tuned for the subset we implement.

### Two-phase codegen strategy

1. **Phase 3A: Spec-driven struct generation** — Generate V1 response types, HTTP body types, and error helpers from a YAML/TOML spec. Low risk, high value.
2. **Phase 3B: Route handler generation** — Generate route handler stubs that call into `handle()`. Medium risk — needs to handle the variety of handler shapes (path params, query params, body, no body, etc.).
3. **Phase 3C: (Optional) OpenAPI import** — Parse real OpenAPI/Discovery Documents into our spec format. High complexity, deferred unless we're building many twins.

---

## 7. Open Questions

1. **Should error response helpers be framework-provided?** Both twins have nearly identical `twin_error_to_v1_response()` / `twin_error_to_v3_response()` functions. A generic version in `twin-server-core` parameterized by API version string could eliminate this duplication.

2. **Should actor ID extraction be an axum extractor?** The 5-line `extract_actor_id` pattern repeats in every V1 route handler. A custom `ActorId` extractor would clean this up.

3. **Should `routes()` be generated from a manifest?** Instead of hand-writing route wiring, a twin could declare routes in a data structure and have the framework build the router. This is the lightest form of codegen.

4. **What's the right spec format for codegen input?** Options: YAML (human-friendly), TOML (Rust-idiomatic), JSON (parseable), or a Rust DSL (macro-based). The spec needs to be readable by both the generator and developers.
