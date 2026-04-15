"""Gmail twin provider."""

from __future__ import annotations

from typing import Any
from urllib.error import URLError

from agent_sandbox_twins._http import get_json, post_json
from agent_sandbox_twins._reshape import reshape_events, reshape_gmail_snapshot


class GmailTwinProvider:
    """Gmail twin provider for agent-sandbox."""

    @property
    def name(self) -> str:
        return "gmail"

    def env_var_name(self) -> str:
        return "AGENT_SANDBOX_TWIN_GMAIL_BASE_URL"

    def default_base_url(self) -> str:
        return "http://localhost:9200"

    def health_check(self, base_url: str) -> None:
        try:
            get_json(f"{base_url}/health")
        except URLError as error:
            raise RuntimeError(f"Gmail twin not reachable at {base_url}") from error

    def reset(self, base_url: str) -> None:
        post_json(f"{base_url}/control/reset", {"seed": 0, "start_time_unix_ms": 0})

    def seed(self, base_url: str, seed_data: dict[str, Any]) -> None:
        post_json(f"{base_url}/control/seed", seed_data or {"messages": []})

    def snapshot(self, base_url: str) -> dict[str, Any]:
        state = get_json(f"{base_url}/control/snapshot")
        return reshape_gmail_snapshot(state)

    def events(self, base_url: str) -> list[dict[str, Any]]:
        raw = get_json(f"{base_url}/control/events")
        return reshape_events(raw)

    def runtime_env_defaults(
        self,
        base_url: str,
        scenario: dict[str, Any],  # noqa: ARG002
    ) -> dict[str, str]:
        return {"AGENT_SANDBOX_TWIN_GMAIL_BASE_URL": base_url}
