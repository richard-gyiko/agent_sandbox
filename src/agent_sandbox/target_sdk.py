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
    twin_gmail_base_url: str
    twin_drive_base_url: str

    @property
    def use_twin(self) -> bool:
        return self.mode == "twin"


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
    """Load runtime config from WHIZY_* environment variables."""
    source = env or os.environ
    raw_mode = source.get("WHIZY_RUNTIME_MODE", default_mode)
    gmail_base = source.get("WHIZY_TWIN_GMAIL_BASE_URL", "http://localhost:9200")
    drive_base = source.get("WHIZY_TWIN_DRIVE_BASE_URL", "http://localhost:9100")
    return RuntimeConfig(
        mode=_normalize_mode(raw_mode),
        twin_gmail_base_url=str(gmail_base),
        twin_drive_base_url=str(drive_base),
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
