"""Runtime config and event hooks for sandbox targets."""

from __future__ import annotations

import os
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass
from typing import TypeAlias


@dataclass(frozen=True)
class RuntimeConfig:
    """Resolved runtime config for sandbox-backed target clients."""

    mode: str
    twin_urls: dict[str, str]

    @property
    def use_twin(self) -> bool:
        return self.mode == "twin"

    def twin_url(self, name: str) -> str:
        """Get a twin's base URL by provider name."""
        return self.twin_urls.get(name, "")

    @property
    def twin_gmail_base_url(self) -> str:
        return self.twin_urls.get("gmail", "http://localhost:9200")

    @property
    def twin_drive_base_url(self) -> str:
        return self.twin_urls.get("drive", "http://localhost:9100")


TargetRuntimeConfig: TypeAlias = RuntimeConfig

_EVENT_JOURNAL: ContextVar[list[dict[str, object]] | None] = ContextVar(
    "agent_sandbox_event_journal",
    default=None,
)


def _normalize_mode(raw: str) -> str:
    value = raw.strip().lower()
    if value in {"twin", "sandbox"}:
        return "twin"
    return "live"


def load_target_runtime_config(
    env: dict[str, str] | None = None,
    *,
    default_mode: str = "live",
) -> TargetRuntimeConfig:
    """Load runtime config from AGENT_SANDBOX_* environment variables."""
    source = env or os.environ
    raw_mode = source.get("AGENT_SANDBOX_RUNTIME_MODE", default_mode)
    twin_urls: dict[str, str] = {}
    from agent_sandbox.twin_provider import get_all_twin_providers

    providers = get_all_twin_providers()
    if providers:
        for name, provider in providers.items():
            twin_urls[name] = source.get(provider.env_var_name(), provider.default_base_url())
    else:
        # No providers registered — scan env directly for twin URLs
        _prefix = "AGENT_SANDBOX_TWIN_"
        _suffix = "_BASE_URL"
        for key, value in source.items():
            if key.startswith(_prefix) and key.endswith(_suffix):
                name = key[len(_prefix) : -len(_suffix)].lower()
                twin_urls[name] = value
    return RuntimeConfig(
        mode=_normalize_mode(raw_mode),
        twin_urls=twin_urls,
    )


def emit_event(kind: str, **attrs: object) -> None:
    """Emit a runtime event into active capture context."""
    journal = _EVENT_JOURNAL.get()
    if journal is None:
        return
    event: dict[str, object] = {"kind": str(kind)}
    if attrs:
        event["attrs"] = dict(attrs)
    journal.append(event)


def current_events() -> list[dict[str, object]]:
    """Return events from active capture context or empty list."""
    journal = _EVENT_JOURNAL.get()
    if journal is None:
        return []
    return [dict(item) for item in journal]


@contextmanager
def capture_events() -> list[dict[str, object]]:
    """Capture runtime events for current execution scope."""
    journal: list[dict[str, object]] = []
    token = _EVENT_JOURNAL.set(journal)
    try:
        yield journal
    finally:
        _EVENT_JOURNAL.reset(token)
