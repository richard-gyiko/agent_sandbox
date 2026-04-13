"""Environment utility helpers for scoped runtime configuration."""

from __future__ import annotations

import contextlib
import os

_MISSING = object()


def is_truthy(value: str | None) -> bool:
    """Parse common truthy environment-string values."""
    if value is None:
        return False
    return value.strip().lower() in {"1", "true", "yes", "on"}


def getenv(name: str, default: str | None = None) -> str | None:
    """Read an environment variable."""
    value = os.getenv(name)
    return value if value is not None else default


@contextlib.contextmanager
def scoped_env(
    *,
    overrides: dict[str, str] | None = None,
    defaults: dict[str, str] | None = None,
):
    """Temporarily apply env var defaults/overrides and restore afterward."""
    previous: dict[str, object] = {}
    try:
        if defaults:
            for key, value in defaults.items():
                if key in os.environ:
                    continue
                previous.setdefault(key, _MISSING)
                os.environ[key] = str(value)
        if overrides:
            for key, value in overrides.items():
                previous.setdefault(key, os.environ.get(key, _MISSING))
                os.environ[key] = str(value)
        yield
    finally:
        for key, value in previous.items():
            if value is _MISSING:
                os.environ.pop(key, None)
            else:
                os.environ[key] = str(value)
