"""Built-in Gmail and Drive twin providers.

These serve as the default providers when no external plugin is installed.
Once the agent-sandbox-twins plugin package is available, these can be removed.
"""

from __future__ import annotations

import json
from typing import Any
from urllib.error import URLError
from urllib.request import Request, urlopen


def _get_json(url: str) -> Any:
    req = Request(url, method="GET")
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))


def _post_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    req = Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))


def _reshape_events(events: Any) -> list[dict[str, Any]]:
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


class GmailTwinProvider:
    """Built-in Gmail twin provider."""

    @property
    def name(self) -> str:
        return "gmail"

    def env_var_name(self) -> str:
        return "AGENT_SANDBOX_TWIN_GMAIL_BASE_URL"

    def default_base_url(self) -> str:
        return "http://localhost:9200"

    def health_check(self, base_url: str) -> None:
        try:
            _get_json(f"{base_url}/health")
        except URLError as error:
            raise RuntimeError(f"Gmail twin not reachable at {base_url}") from error

    def reset(self, base_url: str) -> None:
        _post_json(f"{base_url}/control/reset", {"seed": 0, "start_time_unix_ms": 0})

    def seed(self, base_url: str, seed_data: dict[str, Any]) -> None:
        _post_json(f"{base_url}/control/seed", seed_data or {"messages": []})

    def snapshot(self, base_url: str) -> dict[str, Any]:
        state = _get_json(f"{base_url}/control/snapshot")
        return self._reshape(state)

    def events(self, base_url: str) -> list[dict[str, Any]]:
        raw = _get_json(f"{base_url}/control/events")
        return _reshape_events(raw)

    def runtime_env_defaults(
        self,
        base_url: str,
        scenario: dict[str, Any],  # noqa: ARG002
    ) -> dict[str, str]:
        return {"AGENT_SANDBOX_TWIN_GMAIL_BASE_URL": base_url}

    @staticmethod
    def _reshape(state: dict[str, Any]) -> dict[str, Any]:
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
                labels = [
                    label_names.get(str(lid), str(lid)) for lid in label_ids
                ]
                to_field = message.get("to", [])
                cc_field = message.get("cc", [])
                to_text = (
                    ", ".join(to_field) if isinstance(to_field, list) else str(to_field)
                )
                cc_text = (
                    ", ".join(cc_field) if isinstance(cc_field, list) else str(cc_field)
                )
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


class DriveTwinProvider:
    """Built-in Drive twin provider."""

    @property
    def name(self) -> str:
        return "drive"

    def env_var_name(self) -> str:
        return "AGENT_SANDBOX_TWIN_DRIVE_BASE_URL"

    def default_base_url(self) -> str:
        return "http://localhost:9100"

    def health_check(self, base_url: str) -> None:
        try:
            _get_json(f"{base_url}/health")
        except URLError as error:
            raise RuntimeError(f"Drive twin not reachable at {base_url}") from error

    def reset(self, base_url: str) -> None:
        _post_json(f"{base_url}/control/reset", {"seed": 0, "start_time_unix_ms": 0})

    def seed(self, base_url: str, seed_data: dict[str, Any]) -> None:
        _post_json(f"{base_url}/control/seed", seed_data or {"files": []})

    def snapshot(self, base_url: str) -> dict[str, Any]:
        state = _get_json(f"{base_url}/control/snapshot")
        return self._reshape(state)

    def events(self, base_url: str) -> list[dict[str, Any]]:
        raw = _get_json(f"{base_url}/control/events")
        return _reshape_events(raw)

    def runtime_env_defaults(
        self,
        base_url: str,
        scenario: dict[str, Any],
    ) -> dict[str, str]:
        return {
            "AGENT_SANDBOX_TWIN_DRIVE_BASE_URL": base_url,
            "GDRIVE_ROOT_FOLDER_ID": _resolve_root_folder_id(scenario),
        }

    @staticmethod
    def _reshape(state: dict[str, Any]) -> dict[str, Any]:
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


def _resolve_root_folder_id(scenario: dict[str, Any]) -> str:
    root_id = "root"
    drive_seed = scenario.get("seed", {}).get("drive", {})
    folders = drive_seed.get("folders", [])
    if not folders:
        folders = [
            item
            for item in drive_seed.get("files", [])
            if isinstance(item, dict) and item.get("kind") == "Folder"
        ]
    for folder in folders:
        if folder.get("parent_id") is None:
            root_id = folder.get("id", "root")
            break
    return root_id


def register_builtin_providers() -> None:
    """Register the built-in Gmail and Drive twin providers."""
    from agent_sandbox.twin_provider import register_twin_provider

    register_twin_provider(GmailTwinProvider())
    register_twin_provider(DriveTwinProvider())
