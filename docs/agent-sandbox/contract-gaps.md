# Twin Contract Gaps

This file tracks known behavior differences between local twins and real Gmail/Drive APIs.
Treat these as explicit risk boundaries for lab confidence.

## Gmail Twin

1. Query language coverage is partial.
- Supported tokens: `in:`, `after:`, `before:`, `-label:`
- Unsupported: full Gmail search syntax richness (complex boolean/grouping, many operators)

2. Label model is simplified.
- Labels are stored as plain strings in `message.labels`
- No separate label resource model (IDs, metadata, visibility settings)

3. Message payload is normalized and reduced.
- Twin returns a fixed envelope used by tools (`id`, `thread_id`, bodies, attachments, labels)
- Real Gmail message payload details/headers are not fully mirrored

4. Attachment endpoint semantics are tool-focused.
- Binary attachment retrieval works for seeded blobs
- No Gmail attachment size limits/streaming behavior emulation

## Drive Twin

1. Permission/sharing model is not implemented.
- No ACL roles, sharing scopes, or permission inheritance behavior

2. Metadata and query semantics are simplified.
- Folder/file lookup is explicit endpoint-based, not full Drive query language parity
- Partial metadata surface only (fields required by current tool layer)

3. Upload behavior is minimal.
- Upload accepts base64 payload and stores blob/content locally
- No resumable upload protocol or quota/rate-limit simulation

4. File listing and pagination are reduced.
- `page_size` supported, but no page token flow

## Operational Guidance

1. Keep contract tests aligned with tool-facing behavior only.
2. Add a gap entry whenever a production incident reveals twin divergence.
3. Promote a gap to implementation work once it affects p0 workflow confidence.
