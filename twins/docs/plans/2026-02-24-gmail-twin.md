# Gmail Digital Twin

> Plan for implementing a Gmail API v1 digital twin, validating framework patterns established by twin-drive and informing future OpenAPI codegen (Phase 3).

## Goals

1. Build a fully functional Gmail twin that AI agents can test against
2. Validate that the twin framework (TwinService, TwinSnapshot, StateInspectable, twin-server-core) works cleanly for a second, structurally different service
3. Discover which patterns from twin-drive are truly universal vs Drive-specific
4. Produce a second data point for future OpenAPI codegen design

## Scope

### In scope (Core subset)

| Resource | Operations |
|----------|-----------|
| **Messages** | list, get, send, insert, modify (labels), trash, untrash, delete |
| **Threads** | list, get, modify (labels), trash, untrash, delete |
| **Labels** | list, get, create, update, patch, delete |
| **Attachments** | get (download attachment blob by ID) |
| **Profile** | getProfile (email, totals, historyId) |

### Out of scope (defer to later)

| Feature | Reason |
|---------|--------|
| Drafts | Less common in agent workflows; structurally similar to messages, easy to add later |
| History API | Sync mechanism; agents typically poll via list, not history |
| Batch operations | batchModify/batchDelete are sugar over individual operations |
| Full MIME structure | Agents work with subject/body/from/to, not multipart MIME trees |
| `format=raw` | RFC 2822 encoding is complex and rarely used by agents |
| Settings/Filters | Account management, not relevant to message-level testing |
| `q` search parameter | Complex query parser; stub it initially, expand if needed |

### Simplified message model

Instead of full MIME trees, messages store structured fields:

```rust
pub struct GmailMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub label_ids: Vec<LabelId>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub snippet: String,
    pub internal_date: u64,          // epoch ms
    pub size_estimate: u64,
    pub attachments: Vec<AttachmentRef>,
    pub history_id: u64,
}
```

The `format` parameter on `get` controls which fields are returned:
- `full` (default): all fields including body
- `metadata`: headers + labels, no body
- `minimal`: id + threadId + labelIds only

This is accurate enough for agent testing without implementing recursive MIME part trees.

---

## Architecture

```
crates/twin-gmail/             New crate — all domain logic
  src/lib.rs                   ~1,200-1,500 lines estimated
  Cargo.toml

apps/twin-gmail-server/        Thin binary — identical pattern to twin-drive-server
  src/main.rs                  ~40 lines (scaffolded)
  Cargo.toml
```

Dependencies: `twin-service`, `axum`, `serde`, `serde_json`, `base64`.

---

## Domain Model

### Entities

```rust
type MessageId = String;
type ThreadId = String;
type LabelId = String;
type AttachmentId = String;

pub struct GmailMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub label_ids: Vec<LabelId>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub snippet: String,
    pub internal_date: u64,
    pub size_estimate: u64,
    pub attachments: Vec<AttachmentRef>,
    pub history_id: u64,
}

pub struct AttachmentRef {
    pub attachment_id: AttachmentId,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

pub struct GmailThread {
    pub id: ThreadId,
    pub history_id: u64,
    pub snippet: String,     // from most recent message
}

pub struct GmailLabel {
    pub id: LabelId,
    pub name: String,
    pub label_type: LabelType,
    pub message_list_visibility: Visibility,
    pub label_list_visibility: LabelVisibility,
    pub color: Option<LabelColor>,
}

pub enum LabelType { System, User }

pub struct LabelColor {
    pub text_color: String,
    pub background_color: String,
}
```

### Service struct

```rust
#[derive(Default, Serialize, Deserialize, TwinSnapshot)]
pub struct GmailTwinService {
    messages: BTreeMap<MessageId, GmailMessage>,
    threads: BTreeMap<ThreadId, GmailThread>,
    labels: BTreeMap<LabelId, GmailLabel>,
    #[twin_snapshot(encode = "base64")]
    attachments: BTreeMap<AttachmentId, Vec<u8>>,
    next_id: u64,
    next_history_id: u64,
}
```

### Default state

`Default` impl creates the standard system labels:

| ID | Name |
|----|------|
| `INBOX` | INBOX |
| `SENT` | SENT |
| `DRAFT` | DRAFT |
| `TRASH` | TRASH |
| `SPAM` | SPAM |
| `UNREAD` | UNREAD |
| `STARRED` | STARRED |
| `IMPORTANT` | IMPORTANT |
| `CATEGORY_PERSONAL` | CATEGORY_PERSONAL |
| `CATEGORY_SOCIAL` | CATEGORY_SOCIAL |
| `CATEGORY_PROMOTIONS` | CATEGORY_PROMOTIONS |
| `CATEGORY_UPDATES` | CATEGORY_UPDATES |
| `CATEGORY_FORUMS` | CATEGORY_FORUMS |

---

## Request/Response Pattern

Following the twin-drive pattern of internal enums that decouple domain logic from HTTP:

```rust
pub enum GmailRequest {
    // Messages
    ListMessages { actor_id: String, label_ids: Vec<String>, max_results: u32, page_token: Option<String> },
    GetMessage { actor_id: String, message_id: String, format: MessageFormat },
    SendMessage { actor_id: String, to: Vec<String>, cc: Vec<String>, bcc: Vec<String>, subject: String, body: String, thread_id: Option<String>, attachments: Vec<(String, String, Vec<u8>)> },
    InsertMessage { actor_id: String, message: SeedMessage },
    ModifyMessage { actor_id: String, message_id: String, add_label_ids: Vec<String>, remove_label_ids: Vec<String> },
    TrashMessage { actor_id: String, message_id: String },
    UntrashMessage { actor_id: String, message_id: String },
    DeleteMessage { actor_id: String, message_id: String },

    // Threads
    ListThreads { actor_id: String, label_ids: Vec<String>, max_results: u32, page_token: Option<String> },
    GetThread { actor_id: String, thread_id: String, format: MessageFormat },
    ModifyThread { actor_id: String, thread_id: String, add_label_ids: Vec<String>, remove_label_ids: Vec<String> },
    TrashThread { actor_id: String, thread_id: String },
    UntrashThread { actor_id: String, thread_id: String },
    DeleteThread { actor_id: String, thread_id: String },

    // Labels
    ListLabels { actor_id: String },
    GetLabel { actor_id: String, label_id: String },
    CreateLabel { actor_id: String, name: String, label_list_visibility: Option<String>, message_list_visibility: Option<String> },
    UpdateLabel { actor_id: String, label_id: String, name: Option<String>, label_list_visibility: Option<String>, message_list_visibility: Option<String> },
    DeleteLabel { actor_id: String, label_id: String },

    // Attachments
    GetAttachment { actor_id: String, message_id: String, attachment_id: String },

    // Profile
    GetProfile { actor_id: String },
}

pub enum GmailResponse {
    Message(GmailMessage),
    MessageList { messages: Vec<(MessageId, ThreadId)>, next_page_token: Option<String>, result_size_estimate: u32 },
    Thread(GmailThread, Vec<GmailMessage>),
    ThreadList { threads: Vec<ThreadSummary>, next_page_token: Option<String>, result_size_estimate: u32 },
    Label(GmailLabel),
    LabelList(Vec<GmailLabel>),
    Attachment { data: Vec<u8>, size: u64 },
    Profile { email: String, messages_total: u64, threads_total: u64, history_id: u64 },
    Ok,
    Deleted,
}
```

---

## V1 API Mimicry Routes

All routes prefixed with `/gmail/v1/users/me/`.

The `{userId}` path segment in the real Gmail API is always `me` for our twin (we resolve the actual actor from auth headers, same as Drive).

### Messages

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/gmail/v1/users/me/messages` | `route_v1_list_messages` |
| `GET` | `/gmail/v1/users/me/messages/{id}` | `route_v1_get_message` |
| `POST` | `/gmail/v1/users/me/messages/send` | `route_v1_send_message` |
| `POST` | `/gmail/v1/users/me/messages` | `route_v1_insert_message` |
| `POST` | `/gmail/v1/users/me/messages/{id}/modify` | `route_v1_modify_message` |
| `POST` | `/gmail/v1/users/me/messages/{id}/trash` | `route_v1_trash_message` |
| `POST` | `/gmail/v1/users/me/messages/{id}/untrash` | `route_v1_untrash_message` |
| `DELETE` | `/gmail/v1/users/me/messages/{id}` | `route_v1_delete_message` |

### Threads

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/gmail/v1/users/me/threads` | `route_v1_list_threads` |
| `GET` | `/gmail/v1/users/me/threads/{id}` | `route_v1_get_thread` |
| `POST` | `/gmail/v1/users/me/threads/{id}/modify` | `route_v1_modify_thread` |
| `POST` | `/gmail/v1/users/me/threads/{id}/trash` | `route_v1_trash_thread` |
| `POST` | `/gmail/v1/users/me/threads/{id}/untrash` | `route_v1_untrash_thread` |
| `DELETE` | `/gmail/v1/users/me/threads/{id}` | `route_v1_delete_thread` |

### Labels

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/gmail/v1/users/me/labels` | `route_v1_list_labels` |
| `GET` | `/gmail/v1/users/me/labels/{id}` | `route_v1_get_label` |
| `POST` | `/gmail/v1/users/me/labels` | `route_v1_create_label` |
| `PUT` | `/gmail/v1/users/me/labels/{id}` | `route_v1_update_label` |
| `PATCH` | `/gmail/v1/users/me/labels/{id}` | `route_v1_patch_label` |
| `DELETE` | `/gmail/v1/users/me/labels/{id}` | `route_v1_delete_label` |

### Attachments

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/gmail/v1/users/me/messages/{messageId}/attachments/{id}` | `route_v1_get_attachment` |

### Profile

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/gmail/v1/users/me/profile` | `route_v1_get_profile` |

### Native Routes (twin-specific)

Simpler routes for direct testing without Google API shape overhead:

| Method | Path | Handler |
|--------|------|---------|
| `POST` | `/gmail/messages/send` | `route_send_message` |
| `GET` | `/gmail/messages/{id}` | `route_get_message` |
| `POST` | `/gmail/messages/{id}/labels` | `route_modify_labels` |
| `DELETE` | `/gmail/messages/{id}` | `route_delete_message` |
| `POST` | `/gmail/labels` | `route_create_label` |
| `GET` | `/gmail/labels` | `route_list_labels` |
| `DELETE` | `/gmail/labels/{id}` | `route_delete_label` |
| `GET` | `/gmail/threads/{id}` | `route_get_thread` |

---

## V1 Response Shapes

Matching Gmail API v1 JSON format (camelCase):

### Message (format=full)

```json
{
  "id": "msg_1",
  "threadId": "thread_1",
  "labelIds": ["INBOX", "UNREAD"],
  "snippet": "Hey, can you review...",
  "historyId": "42",
  "internalDate": "1704067200000",
  "sizeEstimate": 1234,
  "payload": {
    "mimeType": "multipart/mixed",
    "headers": [
      { "name": "From", "value": "alice@example.com" },
      { "name": "To", "value": "bob@example.com" },
      { "name": "Subject", "value": "Review request" },
      { "name": "Date", "value": "Mon, 01 Jan 2024 00:00:00 +0000" }
    ],
    "body": { "size": 0 },
    "parts": [
      {
        "partId": "0",
        "mimeType": "text/plain",
        "body": { "size": 42, "data": "<base64url-encoded body>" }
      }
    ]
  }
}
```

Even with simplified internal storage, the V1 response reconstructs the Gmail payload structure so SDK clients see familiar shapes. The `payload.parts` array is synthesized from our flat `body_text`/`body_html`/`attachments` fields.

### Message (format=metadata)

Same as full but `body.data` omitted from all parts. Only headers and labelIds.

### Message (format=minimal)

```json
{ "id": "msg_1", "threadId": "thread_1", "labelIds": ["INBOX", "UNREAD"] }
```

### Message List

```json
{
  "messages": [
    { "id": "msg_1", "threadId": "thread_1" },
    { "id": "msg_2", "threadId": "thread_1" }
  ],
  "nextPageToken": "token_or_absent",
  "resultSizeEstimate": 42
}
```

### Thread

```json
{
  "id": "thread_1",
  "historyId": "42",
  "snippet": "Latest message preview...",
  "messages": [ /* full message objects */ ]
}
```

### Label

```json
{
  "id": "Label_1",
  "name": "Work",
  "type": "user",
  "messageListVisibility": "show",
  "labelListVisibility": "labelShow",
  "messagesTotal": 10,
  "messagesUnread": 3,
  "threadsTotal": 5,
  "threadsUnread": 2
}
```

---

## Scenario Support

### Initial State (seed)

```json
{
  "labels": [
    { "id": "label_1", "name": "Work", "type": "user" }
  ],
  "messages": [
    {
      "id": "msg_1",
      "thread_id": "thread_1",
      "from": "alice@example.com",
      "to": ["bob@example.com"],
      "subject": "Hello",
      "body": "Hi Bob, how are you?",
      "label_ids": ["INBOX", "UNREAD"],
      "timestamp_ms": 1704067200000,
      "attachments": [
        { "filename": "report.pdf", "mime_type": "application/pdf", "content": "<base64>" }
      ]
    },
    {
      "id": "msg_2",
      "thread_id": "thread_1",
      "from": "bob@example.com",
      "to": ["alice@example.com"],
      "subject": "Re: Hello",
      "body": "Doing well!",
      "label_ids": ["INBOX", "UNREAD"],
      "timestamp_ms": 1704070800000
    }
  ]
}
```

Seeding:
1. Create any user labels first (system labels exist from `Default`)
2. Group messages by thread_id, create threads
3. Insert messages, link to threads, assign labels
4. Decode base64 attachment content, store in attachment map
5. Bump `next_id` past all seeded IDs

### Assertions

| Type | Parameters | What it checks |
|------|-----------|----------------|
| `message_exists` | `message_id` | Message with given ID exists |
| `message_has_label` | `message_id`, `label_id` | Message has the specified label |
| `message_not_has_label` | `message_id`, `label_id` | Message does NOT have the label |
| `label_exists` | `label_id` | Label exists |
| `thread_message_count` | `thread_id`, `count` | Thread has exactly N messages |
| `actor_message_count` | `actor_id`, `label_id`, `count` | Actor has N messages with given label |
| `message_in_trash` | `message_id` | Message has TRASH label |

### Timeline Actions

| Type | Parameters | Endpoint (for fault matching) |
|------|-----------|-------------------------------|
| `send_message` | `to`, `subject`, `body`, `thread_id?`, `attachments?` | `/gmail/messages/send` |
| `get_message` | `message_id`, `format?` | `/gmail/messages/{id}` |
| `modify_labels` | `message_id`, `add_label_ids`, `remove_label_ids` | `/gmail/messages/{id}/labels` |
| `trash_message` | `message_id` | `/gmail/messages/{id}/trash` |
| `delete_message` | `message_id` | `/gmail/messages/{id}` |
| `create_label` | `name` | `/gmail/labels` |
| `delete_label` | `label_id` | `/gmail/labels/{id}` |
| `get_thread` | `thread_id` | `/gmail/threads/{id}` |

### Validation

`validate_scenario()` checks:
- All `message.thread_id` references are consistent (messages in same thread share a thread_id)
- All `label_ids` on messages reference valid labels (system or declared in `labels`)
- No duplicate message IDs
- Assertion parameters reference valid actors from the actors list
- Timeline action field validation (required fields present, valid label names)

---

## State Inspection

`StateInspectable` impl maps the domain to a tree:

```
Thread: thread_1 (kind: "thread")
  ├── Message: msg_1 (kind: "message", parent_id: "thread_1")
  │     properties: { from, to, subject, label_ids, has_attachments }
  └── Message: msg_2 (kind: "message", parent_id: "thread_1")

Label: INBOX (kind: "label")
  properties: { type: "system", messages_total, messages_unread }

Label: label_1 (kind: "label")
  properties: { type: "user", messages_total, messages_unread }
```

Threads are top-level nodes, messages are children of threads, labels are separate top-level nodes. This gives a clean two-level hierarchy for the tree view.

---

## Threading Logic

When a message is sent/inserted:
1. If `thread_id` is provided and that thread exists, add to it
2. If no `thread_id`, check if any existing thread has a message with matching subject (simplified thread detection)
3. If no match, create a new thread
4. Update the thread's `snippet` and `history_id`

This is a simplification of Gmail's real threading (which uses Message-ID/In-Reply-To/References headers). Sufficient for testing.

---

## Pagination

Messages and threads support pagination via `page_token` / `next_page_token`:
- Default `max_results`: 100
- Sort by `internal_date` descending (newest first)
- `page_token` is a simple offset token (e.g., `"offset:100"`)
- `result_size_estimate` returns total count matching the query

---

## Key Differences from twin-drive

These are the areas where Gmail will stress-test the framework differently:

| Aspect | Drive | Gmail |
|--------|-------|-------|
| Primary hierarchy | Parent-child folders | Threads -> Messages |
| Access model | Per-item permissions with roles | Per-mailbox (all messages belong to the authenticated user) |
| Identity | Items have owners, shared via permissions | Messages have from/to, but all belong to the mailbox owner |
| List responses | Rich (full item metadata) | Sparse (only id + threadId) |
| Binary content | Per-item content blobs | Per-message attachments (multiple per message) |
| Labeling | N/A (hierarchy is the organization) | Multi-label system (messages can have many labels) |
| Format parameter | N/A | `format=full\|metadata\|minimal` controls response shape |
| System entities | Single root folder | 13 system labels |

---

## Implementation Phases

### Phase 1: Foundation (scaffold + domain model + basic operations) — COMPLETE

| # | Task | Est. lines |
|---|------|-----------|
| 1 | Scaffold crate and server binary using `twin-cli new gmail` | ~40 |
| 2 | Define domain types: `GmailMessage`, `GmailThread`, `GmailLabel`, `AttachmentRef`, enums | ~120 |
| 3 | Define `GmailRequest` / `GmailResponse` enums | ~80 |
| 4 | Implement `GmailTwinService` struct with `Default` (system labels) | ~80 |
| 5 | Implement helper methods: `new_message_id()`, `new_thread_id()`, snippet generation, label counting | ~80 |
| 6 | Implement `handle()` for label operations: ListLabels, GetLabel, CreateLabel, UpdateLabel, DeleteLabel | ~120 |
| 7 | Implement `handle()` for message operations: SendMessage, InsertMessage, GetMessage, ListMessages, ModifyMessage, TrashMessage, UntrashMessage, DeleteMessage | ~250 |
| 8 | Implement `handle()` for thread operations: GetThread, ListThreads, ModifyThread, TrashThread, UntrashThread, DeleteThread | ~150 |
| 9 | Implement `handle()` for GetAttachment + GetProfile | ~40 |
| 10 | Unit tests for all `handle()` operations | ~400 |

### Phase 2: API mimicry (V1 routes) — COMPLETE

| # | Task | Est. lines |
|---|------|-----------|
| 11 | Define V1 response types (`V1Message`, `V1Thread`, `V1Label`, `V1MessageList`, etc.) with `camelCase` serde | ~100 |
| 12 | Implement payload synthesis: `gmail_message_to_v1_payload()` — build the `payload.parts` structure from flat fields | ~80 |
| 13 | Implement V1 message routes (list, get, send, insert, modify, trash, untrash, delete) | ~200 |
| 14 | Implement V1 thread routes (list, get, modify, trash, untrash, delete) | ~120 |
| 15 | Implement V1 label routes (list, get, create, update, patch, delete) | ~100 |
| 16 | Implement V1 attachment + profile routes | ~40 |
| 17 | V1 error response helpers (`v1_error_response`, `twin_error_to_v1_response`) | ~30 |
| 18 | Integration tests for V1 routes (tower::ServiceExt::oneshot) | ~300 |

### Phase 3: Scenarios + framework integration — COMPLETE

| # | Task | Est. lines |
|---|------|-----------|
| 19 | Implement `seed_from_scenario()` with label + message + attachment seeding | ~100 |
| 20 | Implement `evaluate_assertion()` for all assertion types | ~100 |
| 21 | Implement `execute_timeline_action()` for all action types | ~100 |
| 22 | Implement `validate_scenario()` domain validation | ~80 |
| 23 | Implement `StateInspectable` (threads as parents, messages as children, labels as roots) | ~60 |
| 24 | Wire `routes()`: native + V1 + state_inspection_routes | ~40 |
| 25 | Implement native route handlers | ~120 |
| 26 | Snapshot/restore via `#[derive(TwinSnapshot)]` — verify round-trip with attachments | ~10 |
| 27 | E2E integration tests in server binary (scenario apply, API calls, state inspection) | ~200 |

### Phase 4: Polish — COMPLETE

| # | Task | Est. lines |
|---|------|-----------|
| 28 | Pagination for messages and threads | ~50 |
| 29 | `format` parameter support (full, metadata, minimal) on get message/thread | ~40 |
| 30 | `labelIds` filter parameter on list messages/threads | ~30 |
| 31 | Final test sweep — edge cases, error paths | ~100 |

---

## Estimated Totals

| Category | Lines |
|----------|------:|
| Domain model + enums | ~200 |
| Request/Response enums | ~80 |
| Core business logic (handle) | ~560 |
| Helpers | ~80 |
| V1 response types + mapping | ~180 |
| V1 route handlers | ~460 |
| Native route handlers | ~120 |
| Scenario support | ~280 |
| StateInspectable | ~60 |
| Framework wiring | ~50 |
| **Total non-test code** | **~2,070** |
| Tests | ~1,000 |
| **Grand total** | **~3,070** |

For comparison, twin-drive is ~4,224 lines total (~1,940 non-test + ~2,284 tests). Gmail is similar complexity but we benefit from framework improvements (TwinSnapshot, StateInspectable, default methods).

---

## Dependencies

| Dependency | Purpose | Already in workspace? |
|-----------|---------|----------------------|
| `axum` | HTTP framework | Yes |
| `serde` + `serde_json` | Serialization | Yes |
| `base64` | Attachment encoding | Yes |
| `twin-service` | TwinService trait, TwinSnapshot, StateInspectable | Yes |

No new external dependencies needed.

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Gmail API response shapes are complex even simplified | Medium | Medium | Start with minimal format, expand as needed |
| Threading logic edge cases | Low | Low | Simplify to subject matching, skip RFC 2822 headers |
| `twin-cli new gmail` produces a broken scaffold | Low | Low | We already fixed the scaffolding in Phase 2B |
| Framework doesn't accommodate Gmail's multi-label model | Low | Medium | Framework is generic — labels are just twin-specific state |
| Attachment storage bloats snapshots | Low | Low | Same pattern as Drive content — base64 in TwinSnapshot works |

---

## Success Criteria — All Met

- [x] `twin-cli new gmail` scaffolds, then manual implementation fills in domain logic
- [x] All V1 mimicry routes return Google-shaped JSON (camelCase, correct status codes)
- [x] Scenarios can seed a mailbox, run timeline actions, and verify assertions
- [x] State inspection shows threads → messages hierarchy and label metadata
- [x] Snapshot/restore round-trips correctly including attachments
- [x] The framework (twin-server-core) requires ZERO changes — Gmail plugs in via the same generic `TwinService` interface *(one change was needed — see Framework Validation Checklist)*
- [x] Lessons learned documented for OpenAPI codegen Phase 3

---

## Framework Validation Checklist

All framework claims verified during implementation:

- [x] `TwinService` trait works with no modifications for a non-Drive twin
- [x] `#[derive(TwinSnapshot)]` with `encode = "base64"` works for attachment blobs
- [x] `StateInspectable` + `state_inspection_routes()` produces a valid tree from thread → message hierarchy
- [x] `twin-server-core` auth middleware resolves actor IDs correctly for Gmail routes
- [x] Scenario system (seed, assertions, timeline, validation) works for Gmail domain
- [x] Session management, event logging, and fault injection work without changes
- [x] Scaffolding CLI generates a valid starting point

### Framework Change Required

One framework change was needed: `Action` and `AssertionCheck` in `twin-scenario` were fixed enums with only Drive-specific variants, blocking Gmail timeline actions from being parsed. Both fields were made opaque (`serde_json::Value`) so any twin can define its own action and assertion schemas. The `initial_state` field was already opaque. This change was included in commit `1d1f02f`.

---

## Estimates vs Actuals

| Category | Estimated | Actual | Notes |
|----------|----------:|-------:|-------|
| Domain model + enums | 200 | 97 | Flat fields instead of deep MIME trees |
| Request/Response enums | 80 | 160 | More operations than estimated |
| Service struct + Default | — | 56 | 13 system labels in Default |
| Core business logic (handle) | 560 | 645 | Threading, pagination, format modes |
| Helpers | 80 | 190 | Snippet gen, pagination, date formatting |
| V1 response types + mapping | 180 | 358 | Payload synthesis more complex than expected |
| V1 route handlers | 460 | 616 | More routes, auth extraction boilerplate |
| Native route handlers | 120 | 150 | Close to estimate |
| Scenario support | 280 | 365 | Assertion types richer than planned |
| StateInspectable | 60 | 176 | Thread→message hierarchy more involved |
| Framework wiring (routes) | 50 | 107 | Many more endpoints to wire |
| **Total non-test code** | **2,070** | **3,177** | **53% over estimate** |
| Unit tests | 1,000 | 1,134 | Close to estimate |
| **Grand total** | **3,070** | **4,311** | **40% over estimate** |

The main underestimations were in V1 response mapping (payload synthesis from flat fields to nested Google-style JSON) and the number of route handlers needed. The domain model itself was smaller than expected.
