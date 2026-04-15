"""Snapshot reshaping for Gmail and Drive twin state."""

from __future__ import annotations

from typing import Any


def reshape_gmail_snapshot(state: dict[str, Any]) -> dict[str, Any]:
    service_state = state.get("service_state", {})
    messages_map = service_state.get("messages", {})
    labels_map = service_state.get("labels", {})

    label_names: dict[str, str] = {}
    if isinstance(labels_map, dict):
        for label_id, label in labels_map.items():
            if isinstance(label, dict):
                label_names[str(label_id)] = str(label.get("name", label_id))

    messages: list[dict[str, Any]] = []
    if isinstance(messages_map, dict):
        for message_id, message in messages_map.items():
            if not isinstance(message, dict):
                continue
            label_ids = message.get("label_ids", [])
            labels = [label_names.get(str(lid), str(lid)) for lid in label_ids]
            to_field = message.get("to", [])
            cc_field = message.get("cc", [])
            to_text = ", ".join(to_field) if isinstance(to_field, list) else str(to_field)
            cc_text = ", ".join(cc_field) if isinstance(cc_field, list) else str(cc_field)
            messages.append(
                {
                    "id": str(message.get("id", message_id)),
                    "thread_id": str(message.get("thread_id", "")),
                    "subject": message.get("subject"),
                    "sender": message.get("from"),
                    "to": to_text,
                    "cc": cc_text,
                    "date": message.get("internal_date"),
                    "snippet": message.get("snippet"),
                    "body_plain": message.get("body_text"),
                    "body_html": message.get("body_html"),
                    "labels": labels,
                    "attachments": message.get("attachments", []),
                }
            )
    return {"messages": messages}


def reshape_drive_snapshot(state: dict[str, Any]) -> dict[str, Any]:
    service_state = state.get("service_state", {})
    items = service_state.get("items", {})

    folders: list[dict[str, Any]] = []
    files: list[dict[str, Any]] = []

    if isinstance(items, dict):
        for item_id, item in items.items():
            if not isinstance(item, dict):
                continue
            base = {
                "id": str(item.get("id", item_id)),
                "name": item.get("name"),
                "parent_id": item.get("parent_id"),
            }
            kind = str(item.get("kind", ""))
            if kind == "Folder":
                folders.append(base)
                continue
            files.append(
                {
                    **base,
                    "mime_type": item.get("mime_type"),
                    "app_properties": item.get("app_properties", {}),
                }
            )

    return {"folders": folders, "files": files}


def reshape_events(events: Any) -> list[dict[str, Any]]:
    if not isinstance(events, list):
        return []
    reshaped: list[dict[str, Any]] = []
    for event in events:
        if not isinstance(event, dict):
            continue
        endpoint = str(event.get("endpoint", ""))
        service = endpoint.split("/")[1] if "/" in endpoint else endpoint
        operation = event.get("operation")
        detail = event.get("detail")
        reshaped.append(
            {
                "ts": event.get("logical_time_unix_ms"),
                "service": service,
                "action": operation or detail,
                "request_id": event.get("request_id"),
                "trace_id": event.get("trace_id"),
                "request": {},
                "status": event.get("outcome"),
            }
        )
    return reshaped
