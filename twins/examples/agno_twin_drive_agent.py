#!/usr/bin/env python3
"""
Real Agno agent interacting with the Drive twin.

This example uses:
- Agno Agent + OpenAIChat model
- Custom tool functions that call the twin's Drive compatibility API

Run:
  1) cargo run --bin twin-server
  2) set OPENAI_API_KEY
  3) uv run --with agno --with openai examples/agno_twin_drive_agent.py
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

from agno.agent import Agent
from agno.models.openai import OpenAIChat


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
            with urllib.request.urlopen(req, timeout=20) as resp:
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

    def create_folder(self, actor_id: str, parent_id: str, name: str) -> dict:
        return self._request(
            "POST",
            "/drive/folders",
            {"actor_id": actor_id, "parent_id": parent_id, "name": name},
        )

    def create_file(self, actor_id: str, parent_id: str, name: str) -> dict:
        return self._request(
            "POST",
            "/drive/files",
            {"actor_id": actor_id, "parent_id": parent_id, "name": name},
        )

    def list_children(self, actor_id: str, parent_id: str) -> dict:
        query = urllib.parse.urlencode({"actor_id": actor_id})
        return self._request("GET", f"/drive/items/{parent_id}/children?{query}")

    def move_item(self, actor_id: str, item_id: str, new_parent_id: str) -> dict:
        return self._request(
            "POST",
            f"/drive/items/{item_id}/move",
            {"actor_id": actor_id, "new_parent_id": new_parent_id},
        )


class TwinGoogleDriveTools:
    """Google Drive-like tools backed by the twin API."""

    def __init__(self, client: TwinDriveClient, actor_id: str) -> None:
        self.client = client
        self.actor_id = actor_id

    def list_children(self, parent_id: str = "root") -> list[dict]:
        """List files/folders under a parent folder id."""
        payload = self.client.list_children(self.actor_id, parent_id)
        return payload.get("Listed", {}).get("items", [])

    def create_folder(self, parent_id: str, name: str) -> dict:
        """Create a folder under parent_id."""
        payload = self.client.create_folder(self.actor_id, parent_id, name)
        return payload.get("Created", {}).get("item", {})

    def move_item(self, item_id: str, new_parent_id: str) -> dict:
        """Move an item into a different parent folder."""
        payload = self.client.move_item(self.actor_id, item_id, new_parent_id)
        return payload.get("Updated", {}).get("item", {})


def bootstrap_scenario(actor_id: str) -> dict[str, Any]:
    return {
        "version": 1,
        "name": "agno-twin-bootstrap",
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
        "timeline": [
            {
                "at_ms": 1000,
                "actor_id": actor_id,
                "action": {"type": "create_file", "parent_id": "root", "name": "invoice_jan.pdf"},
            },
            {
                "at_ms": 1100,
                "actor_id": actor_id,
                "action": {"type": "create_file", "parent_id": "root", "name": "Q1_report.txt"},
            },
            {
                "at_ms": 1200,
                "actor_id": actor_id,
                "action": {"type": "create_file", "parent_id": "root", "name": "ideas_notes.md"},
            },
        ],
        "faults": [],
        "assertions": [],
    }


def print_tree(client: TwinDriveClient, actor_id: str, root_id: str = "root") -> None:
    items = client.list_children(actor_id, root_id).get("Listed", {}).get("items", [])
    for item in items:
        print(f"- {item['name']} ({item['kind']}) [{item['id']}]")
        if item["kind"] == "Folder":
            children = (
                client.list_children(actor_id, item["id"]).get("Listed", {}).get("items", [])
            )
            for child in children:
                print(f"  - {child['name']} ({child['kind']}) [{child['id']}]")


def main() -> None:
    if not os.getenv("OPENAI_API_KEY"):
        raise SystemExit("OPENAI_API_KEY is required for Agno Agent model calls")

    actor_id = "alice"
    client = TwinDriveClient("http://127.0.0.1:8080")
    client.reset()
    client.apply_scenario(bootstrap_scenario(actor_id))

    tools = TwinGoogleDriveTools(client, actor_id)
    model_id = os.getenv("OPENAI_MODEL", "gpt-4o-mini")

    agent = Agent(
        name="TwinDriveOrganizer",
        model=OpenAIChat(id=model_id),
        tools=[tools.list_children, tools.create_folder, tools.move_item],
        instructions=[
            "You organize a Google Drive-like file tree.",
            "Use tools only.",
            "Create folders under root as needed: Invoices, Reports, Notes.",
            "Move each root file into exactly one destination folder.",
            "Do not rename files.",
            "When done, provide a short summary.",
        ],
        markdown=False,
    )

    prompt = (
        "Reorganize files currently in root.\n"
        "Rules:\n"
        "- invoice/pdf => Invoices\n"
        "- report/q* => Reports\n"
        "- otherwise => Notes\n"
    )
    result = agent.run(prompt)
    print("\nAgent output:")
    print(result.content)

    print("\nFinal tree:")
    print_tree(client, actor_id)


if __name__ == "__main__":
    main()
