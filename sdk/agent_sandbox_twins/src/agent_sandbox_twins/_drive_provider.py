"""Drive twin provider."""

from __future__ import annotations

from typing import Any
from urllib.error import URLError

from agent_sandbox_twins._http import get_json, post_json
from agent_sandbox_twins._reshape import reshape_drive_snapshot, reshape_events


class DriveTwinProvider:
    """Drive twin provider for agent-sandbox."""

    @property
    def name(self) -> str:
        return "drive"

    def env_var_name(self) -> str:
        return "AGENT_SANDBOX_TWIN_DRIVE_BASE_URL"

    def default_base_url(self) -> str:
        return "http://localhost:9100"

    def health_check(self, base_url: str) -> None:
        try:
            get_json(f"{base_url}/health")
        except URLError as error:
            raise RuntimeError(f"Drive twin not reachable at {base_url}") from error

    def reset(self, base_url: str) -> None:
        post_json(f"{base_url}/control/reset", {"seed": 0, "start_time_unix_ms": 0})

    def seed(self, base_url: str, seed_data: dict[str, Any]) -> None:
        post_json(f"{base_url}/control/seed", seed_data or {"files": []})

    def snapshot(self, base_url: str) -> dict[str, Any]:
        state = get_json(f"{base_url}/control/snapshot")
        return reshape_drive_snapshot(state)

    def events(self, base_url: str) -> list[dict[str, Any]]:
        raw = get_json(f"{base_url}/control/events")
        return reshape_events(raw)

    def runtime_env_defaults(
        self,
        base_url: str,
        scenario: dict[str, Any],
    ) -> dict[str, str]:
        return {
            "AGENT_SANDBOX_TWIN_DRIVE_BASE_URL": base_url,
            "GDRIVE_ROOT_FOLDER_ID": _resolve_root_folder_id(scenario),
        }


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
