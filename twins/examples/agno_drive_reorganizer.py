#!/usr/bin/env python3
"""
Example: Agno-style agent workflow against the Drive twin.

This script is intentionally dependency-light and runnable with stdlib only.
It demonstrates:
1) Resetting the twin to deterministic state
2) Agent-like decision loop (classify file -> choose destination folder)
3) Using Drive compatibility APIs to build organized structure

Note:
- Current twin routes support create/list/permission operations.
- No move endpoint exists yet, so this example organizes incoming files.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request


class TwinHttpError(RuntimeError):
    pass


class TwinDriveClient:
    def __init__(self, base_url: str) -> None:
        self.base_url = base_url.rstrip("/")

    def _request(self, method: str, path: str, body: dict | None = None) -> dict:
        url = f"{self.base_url}{path}"
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["content-type"] = "application/json"
        req = urllib.request.Request(url=url, method=method, data=data, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                payload = resp.read().decode("utf-8")
                return json.loads(payload) if payload else {}
        except urllib.error.HTTPError as exc:
            payload = exc.read().decode("utf-8")
            raise TwinHttpError(f"{exc.code} {payload}") from exc
        except urllib.error.URLError as exc:
            raise TwinHttpError(
                f"Cannot reach twin server at {self.base_url}. "
                "Start it with: cargo run --bin twin-server"
            ) from exc

    def reset(self, seed: int = 42, start_time_unix_ms: int = 1704067200000) -> None:
        self._request(
            "POST",
            "/control/reset",
            {"seed": seed, "start_time_unix_ms": start_time_unix_ms},
        )

    def apply_scenario(self, scenario: dict) -> dict:
        return self._request("POST", "/control/scenario/apply", scenario)

    def create_folder(self, actor_id: str, parent_id: str, name: str) -> str:
        payload = self._request(
            "POST",
            "/drive/folders",
            {"actor_id": actor_id, "parent_id": parent_id, "name": name},
        )
        return payload["Created"]["item"]["id"]

    def create_file(self, actor_id: str, parent_id: str, name: str) -> str:
        payload = self._request(
            "POST",
            "/drive/files",
            {"actor_id": actor_id, "parent_id": parent_id, "name": name},
        )
        return payload["Created"]["item"]["id"]

    def list_children(self, actor_id: str, parent_id: str) -> list[dict]:
        query = urllib.parse.urlencode({"actor_id": actor_id})
        payload = self._request("GET", f"/drive/items/{parent_id}/children?{query}")
        return payload["Listed"]["items"]


class ReorgAgent:
    """
    Replace this class with your Agno agent call.

    Example integration point:
    - Input: filename + current tree summary
    - Output: destination folder label
    """

    def choose_folder(self, filename: str) -> str:
        lower = filename.lower()
        if "invoice" in lower or lower.endswith(".pdf"):
            return "Invoices"
        if "report" in lower or "q" in lower:
            return "Reports"
        return "Notes"


def bootstrap_owner_scenario(actor_id: str) -> dict:
    return {
        "version": 1,
        "name": "agno-reorg-bootstrap",
        "seed": 42,
        "start_time_unix_ms": 1704067200000,
        "actors": [{"id": actor_id, "label": "Agent"}],
        "initial_state": {
            "files": [
                {
                    "id": "root",
                    "name": "My Drive",
                    "parent_id": None,
                    "owner_id": actor_id,
                    "kind": "Folder",
                }
            ]
        },
        "timeline": [],
        "faults": [],
        "assertions": [],
    }


def print_tree(client: TwinDriveClient, actor_id: str, root_id: str = "root") -> None:
    for item in client.list_children(actor_id, root_id):
        print(f"- {item['name']} ({item['kind']}) [{item['id']}]")
        if item["kind"] == "Folder":
            for child in client.list_children(actor_id, item["id"]):
                print(f"  - {child['name']} ({child['kind']}) [{child['id']}]")


def main() -> None:
    base_url = "http://127.0.0.1:8080"
    actor_id = "alice"
    incoming_files = [
        "Q1_report.txt",
        "invoice_2026_01.pdf",
        "brainstorm_notes.md",
        "Q2_report.txt",
    ]

    client = TwinDriveClient(base_url)
    client.reset()
    client.apply_scenario(bootstrap_owner_scenario(actor_id))

    agent = ReorgAgent()
    folders: dict[str, str] = {}

    for filename in incoming_files:
        label = agent.choose_folder(filename)
        folder_id = folders.get(label)
        if folder_id is None:
            folder_id = client.create_folder(actor_id, "root", label)
            folders[label] = folder_id
        file_id = client.create_file(actor_id, folder_id, filename)
        print(f"created {filename} -> {label} ({file_id})")

    print("\nFinal tree:")
    print_tree(client, actor_id)


if __name__ == "__main__":
    main()
