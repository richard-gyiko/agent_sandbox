# Event Operation Contract

Canonical values for asserting operations emitted by twin `/control/events`.

| Endpoint pattern | operation | resource example | Notes |
|---|---|---|---|
| `GET /drive/v3/files` | `GET` | `/drive/v3/files` | `files.list` |
| `GET /drive/v3/files/{file_id}` | `GET` | `/drive/v3/files/item_1` | Metadata fetch (`alt=media` still records `GET`) |
| `POST /drive/v3/files` | `POST` | `/drive/v3/files` | Metadata create |
| `PATCH /drive/v3/files/{file_id}` | `PATCH` | `/drive/v3/files/item_1` | Update / move |
| `DELETE /drive/v3/files/{file_id}` | `DELETE` | `/drive/v3/files/item_1` | Delete |
| `POST /upload/drive/v3/files` | `POST` | `/upload/drive/v3/files` | Media/multipart upload init |
| `PUT /upload/drive/v3/files` | `PUT` | `/upload/drive/v3/files` | Resumable chunk upload |
| `GET /gmail/v1/users/me/messages` | `GET` | `/gmail/v1/users/me/messages` | List/search messages |
| `GET /gmail/v1/users/me/messages/{id}` | `GET` | `/gmail/v1/users/me/messages/msg_1` | Get message |
| `POST /gmail/v1/users/me/messages/send` | `POST` | `/gmail/v1/users/me/messages/send` | Send message |
| `POST /gmail/v1/users/me/messages/{id}/modify` | `POST` | `/gmail/v1/users/me/messages/msg_1/modify` | Label modify |
| `POST /gmail/v1/users/me/messages/{id}/trash` | `POST` | `/gmail/v1/users/me/messages/msg_1/trash` | Trash message |
| `POST /gmail/v1/users/me/messages/{id}/untrash` | `POST` | `/gmail/v1/users/me/messages/msg_1/untrash` | Untrash message |
| `DELETE /gmail/v1/users/me/messages/{id}` | `DELETE` | `/gmail/v1/users/me/messages/msg_1` | Delete message |

## Adapter Mapping

`_reshape_events` in `src/agent_sandbox/runner.py` maps each twin event to:

`{"ts": logical_time_unix_ms, "service": endpoint.split("/")[1], "action": operation or detail, "request_id": request_id, "trace_id": trace_id, "request": {}, "status": outcome}`
