# Drive Twin: File Content Support (Upload & Download)

> Plan for adding in-memory blob storage, simple upload, and `alt=media` download to the Google Drive twin.

## Design Decisions (already approved)

| Decision | Choice | Rationale |
|---|---|---|
| Storage | In-memory `BTreeMap<ItemId, Vec<u8>>` in `DriveTwinService` | Simple, test-sized files, included in snapshots |
| Upload | `POST /upload/drive/v3/files?uploadType=media` | What SDK clients use for small files (<5MB), body = raw bytes |
| Download | `GET /drive/v3/files/{id}?alt=media` | Returns raw bytes with `Content-Type` header |
| Snapshot encoding | Base64-encoded blobs in JSON | Keeps snapshots self-contained and JSON-compatible |

---

## Tasks

### Task 1: Add `mime_type` and `size` fields to `DriveItem`

**File:** `crates/twin-drive/src/lib.rs:106`

Add two optional fields to `DriveItem`:

```rust
pub struct DriveItem {
    pub id: ItemId,
    pub name: String,
    pub kind: DriveItemKind,
    pub parent_id: Option<ItemId>,
    pub owner_id: ActorId,
    pub permissions: Vec<Permission>,
    pub revision: u64,
    pub mime_type: Option<String>,   // NEW
    pub size: Option<u64>,           // NEW — byte count, None if no content
}
```

Update all sites that construct `DriveItem` (CreateFile, CreateFolder, seed_item, default root) to set `mime_type: None, size: None` initially. CreateFolder should set `mime_type: Some("application/vnd.google-apps.folder".into())`.

---

### Task 2: Add content store to `DriveTwinService`

**File:** `crates/twin-drive/src/lib.rs:170`

```rust
pub struct DriveTwinService {
    items: BTreeMap<ItemId, DriveItem>,
    content: BTreeMap<ItemId, Vec<u8>>,  // NEW — blob store
    next_id: u64,
}
```

Update `Default::default()` to initialize `content: BTreeMap::new()`.

---

### Task 3: Add `UploadContent` and `DownloadContent` request/response variants

**File:** `crates/twin-drive/src/lib.rs:117,161`

```rust
pub enum DriveRequest {
    // ... existing variants ...
    UploadContent {
        actor_id: ActorId,
        parent_id: ItemId,
        name: String,
        mime_type: Option<String>,
        content: Vec<u8>,
    },
    DownloadContent {
        actor_id: ActorId,
        item_id: ItemId,
    },
}

pub enum DriveResponse {
    // ... existing variants ...
    ContentCreated { item: DriveItem, size: u64 },
    Content { item: DriveItem, data: Vec<u8> },
}
```

---

### Task 4: Implement `UploadContent` handler in `DriveTwinService::handle()`

**File:** `crates/twin-drive/src/lib.rs` (inside `handle()`)

Logic:
1. Verify `parent_id` exists and is a folder.
2. Check actor has Editor permission on the parent.
3. Generate a new file ID (like `CreateFile`).
4. Create `DriveItem` with `mime_type` set (default `application/octet-stream` if None), `size: Some(content.len() as u64)`.
5. Insert into `items` and `content` stores.
6. Return `ContentCreated { item, size }`.

---

### Task 5: Implement `DownloadContent` handler in `DriveTwinService::handle()`

**File:** `crates/twin-drive/src/lib.rs` (inside `handle()`)

Logic:
1. Look up item by `item_id`.
2. Check actor has at least Viewer permission.
3. Look up content in blob store. If no content exists, return an error (`"file has no content"`).
4. Return `Content { item, data }`.

---

### Task 6: Clean up content on `DeleteItem`

**File:** `crates/twin-drive/src/lib.rs:572`

In the existing `DeleteItem` handler, after the BFS cascade-delete loop collects `to_delete`, also remove each ID from `self.content`:

```rust
for id in &to_delete {
    self.items.remove(id);
    self.content.remove(id);  // NEW
}
```

---

### Task 7: Update snapshot/restore to include content store

**File:** `crates/twin-drive/src/lib.rs:832`

**`service_snapshot()`:**
```rust
fn service_snapshot(&self) -> serde_json::Value {
    let encoded_content: BTreeMap<&str, String> = self
        .content
        .iter()
        .map(|(k, v)| (k.as_str(), base64::engine::general_purpose::STANDARD.encode(v)))
        .collect();
    serde_json::json!({
        "items": self.items,
        "next_id": self.next_id,
        "content": encoded_content,
    })
}
```

**`service_restore()`:** Decode the `"content"` key (if present) from base64 back into `Vec<u8>`. Treat missing `"content"` key as empty map (backward compat with existing snapshots that lack it).

**Dependency:** Add `base64` crate to `twin-drive/Cargo.toml`.

---

### Task 8: Update `V3File` and `drive_item_to_v3_file()`

**File:** `crates/twin-drive/src/lib.rs:59,677`

Add `size` field to `V3File`:

```rust
struct V3File {
    kind: String,
    id: String,
    name: String,
    mime_type: String,
    parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,  // NEW — Google API returns size as a string
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<Vec<V3Permission>>,
}
```

Update `drive_item_to_v3_file()` to:
- Use `item.mime_type.clone().unwrap_or_else(|| ...)` instead of hardcoding. Fall back to `application/vnd.google-apps.folder` for folders and `application/octet-stream` for files.
- Set `size: item.size.map(|s| s.to_string())`.

---

### Task 9: Add upload route (`POST /upload/drive/v3/files`)

**File:** `crates/twin-drive/src/lib.rs` (new route function + registration)

Route: `POST /upload/drive/v3/files?uploadType=media`

Query params: `uploadType` (required, must be `"media"`), `name` (optional), `mimeType` (optional).

Handler:
1. Read `Content-Type` header as the file's MIME type if `mimeType` query param not set.
2. Read request body as raw bytes (`axum::body::Bytes`).
3. Build `DriveRequest::UploadContent` and dispatch to `handle()`.
4. Return `201 Created` with V3File JSON response.

Register in `routes()`:
```rust
.route("/upload/drive/v3/files", post(route_v3_upload_file))
```

---

### Task 10: Add download support to existing get-file route

**File:** `crates/twin-drive/src/lib.rs:1463`

Modify `route_v3_get_file()` to check for `alt=media` query parameter:
- If `alt=media`: dispatch `DriveRequest::DownloadContent`, return raw bytes with `Content-Type` from item's mime_type and `Content-Length` header.
- Otherwise: existing behavior (return JSON metadata).

Add a query struct:
```rust
#[derive(Deserialize)]
struct V3GetFileQuery {
    alt: Option<String>,
    // fields param for future use
}
```

---

### Task 11: Update `SeedFile` and `seed_from_scenario()` for content

**File:** `crates/twin-drive/src/lib.rs:779,858`

Add optional `content` (base64 string) and `mime_type` fields to `SeedFile`:

```rust
struct SeedFile {
    id: String,
    name: String,
    parent_id: Option<String>,
    owner_id: String,
    kind: SeedItemKind,
    mime_type: Option<String>,     // NEW
    content: Option<String>,       // NEW — base64-encoded
}
```

In `seed_from_scenario()`, after inserting the item, decode and store content if present. Set `size` and `mime_type` on the `DriveItem`.

---

### Task 12: Update state inspection endpoints

**File:** `crates/twin-drive/src/lib.rs` (state routes)

The `/state/items` and `/state/items/{id}` responses already serialize `DriveItem`. Since we added `mime_type` and `size` fields, they'll appear automatically.

Add a `has_content: bool` field to the state inspection response (or include it as a computed field) so callers can tell which files have blob data without downloading. Also include `content_size` for clarity.

---

### Task 13: Unit tests

Add tests in the existing test module:

1. **Upload and download round-trip** — upload content, download it, verify bytes match.
2. **Upload creates file with correct metadata** — verify `size`, `mime_type` fields.
3. **Download non-existent content returns error** — create a file without content, attempt download.
4. **Delete cascades to content store** — upload content, delete parent folder, verify content is gone.
5. **Snapshot includes content** — upload content, snapshot, restore, download, verify bytes match.
6. **Seed with content** — seed a file with base64 content, download it, verify.

---

### Task 14: Integration tests for v3 routes

Add integration tests:

1. **`POST /upload/drive/v3/files?uploadType=media`** — send raw bytes, verify 201 + V3File response with size.
2. **`GET /drive/v3/files/{id}?alt=media`** — upload then download via HTTP, verify raw bytes + Content-Type.
3. **`GET /drive/v3/files/{id}`** (without alt=media) — verify still returns JSON metadata with size field.
4. **Upload then list** — upload a file, list parent's children, verify file appears with size.

---

### Task 15: Update ARCHITECTURE.md

Document the content support:
- In-memory blob store design
- Upload/download API surface
- Snapshot format with base64-encoded content

---

## Implementation Order

Tasks 1-2 first (model changes), then 3-6 (domain logic), then 7 (snapshots), then 8-10 (HTTP layer), then 11 (seeding), then 12 (state inspection), then 13-14 (tests), then 15 (docs).

The natural groupings for commits:

| Commit | Tasks | Description |
|---|---|---|
| 1 | 1, 2, 3 | Domain model: add fields, content store, request/response variants |
| 2 | 4, 5, 6 | Domain logic: upload, download, delete-cascade handlers |
| 3 | 7 | Snapshot/restore with base64 content |
| 4 | 8, 9, 10 | HTTP layer: V3File changes, upload route, download route |
| 5 | 11 | Seed support for file content |
| 6 | 12 | State inspection updates |
| 7 | 13, 14 | Tests |
| 8 | 15 | Documentation |

## Dependencies

- `base64` crate — add to `crates/twin-drive/Cargo.toml`
- No other new dependencies needed. `axum::body::Bytes` is already available.
