"""Twin provider protocol and registry for AgentSandbox."""

from __future__ import annotations

from typing import Any, Protocol, runtime_checkable


@runtime_checkable
class TwinProvider(Protocol):
    """Contract that twin backend plugins implement."""

    @property
    def name(self) -> str:
        """Unique name matching seed/snapshot keys (e.g. 'gmail', 'drive')."""
        ...

    def health_check(self, base_url: str) -> None:
        """Raise if the twin is not reachable."""
        ...

    def reset(self, base_url: str) -> None:
        """Reset twin to empty state."""
        ...

    def seed(self, base_url: str, seed_data: dict[str, Any]) -> None:
        """Seed twin with scenario data. seed_data is scenario.seed[name]."""
        ...

    def snapshot(self, base_url: str) -> dict[str, Any]:
        """Return reshaped snapshot state dict."""
        ...

    def events(self, base_url: str) -> list[dict[str, Any]]:
        """Return reshaped operation events list."""
        ...

    def runtime_env_defaults(
        self,
        base_url: str,
        scenario: dict[str, Any],
    ) -> dict[str, str]:
        """Return env var defaults this twin needs set at runtime."""
        ...

    def env_var_name(self) -> str:
        """Return the base URL env var name (e.g. AGENT_SANDBOX_TWIN_GMAIL_BASE_URL)."""
        ...

    def default_base_url(self) -> str:
        """Return the default base URL when no env/config is set."""
        ...


_TWIN_PROVIDERS: dict[str, TwinProvider] = {}


def register_twin_provider(provider: TwinProvider) -> None:
    """Register a twin provider by its name."""
    _TWIN_PROVIDERS[provider.name] = provider


def get_twin_provider(name: str) -> TwinProvider | None:
    """Look up a registered twin provider."""
    return _TWIN_PROVIDERS.get(name)


def list_twin_providers() -> list[str]:
    """Return sorted list of registered twin provider names."""
    return sorted(_TWIN_PROVIDERS.keys())


def get_all_twin_providers() -> dict[str, TwinProvider]:
    """Return all registered twin providers."""
    return dict(_TWIN_PROVIDERS)
