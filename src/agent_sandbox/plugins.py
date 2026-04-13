"""Execution plugin loading and policy controls for AgentSandbox."""

from __future__ import annotations

import importlib

from agent_sandbox.env import getenv, is_truthy

_LOADED_PLUGIN_MODULES: set[str] = set()


def unsafe_plugins_enabled() -> bool:
    value = getenv("AGENT_SANDBOX_UNSAFE_PLUGINS")
    return is_truthy(value)


def plugin_allowlist() -> set[str]:
    raw = getenv("AGENT_SANDBOX_PLUGIN_ALLOWLIST", default="") or ""
    return {item.strip() for item in raw.split(",") if item.strip()}


def list_loaded_modules() -> list[str]:
    return sorted(_LOADED_PLUGIN_MODULES)


def load_execution_plugins(module_names: list[str]) -> None:
    """Load plugin modules that self-register execution handlers."""
    load_execution_plugins_with_importer(module_names, import_module_fn=importlib.import_module)


def load_execution_plugins_with_importer(
    module_names: list[str],
    *,
    import_module_fn,
) -> None:
    """Load plugin modules with a custom importer (test/compat hook)."""
    allowlist = plugin_allowlist()
    unsafe = unsafe_plugins_enabled()
    for module_name in module_names:
        name = module_name.strip()
        if not name:
            continue
        if name in _LOADED_PLUGIN_MODULES:
            continue
        if not unsafe:
            if not allowlist:
                raise ValueError(
                    f"Plugin loading blocked for '{name}'. "
                    "Set AGENT_SANDBOX_PLUGIN_ALLOWLIST or enable "
                    "AGENT_SANDBOX_UNSAFE_PLUGINS=true."
                )
            if name not in allowlist:
                allowed = ", ".join(sorted(allowlist))
                raise ValueError(
                    f"Plugin '{name}' is not in AGENT_SANDBOX_PLUGIN_ALLOWLIST. "
                    f"Allowed: {allowed}"
                )
        import_module_fn(name)
        _LOADED_PLUGIN_MODULES.add(name)
