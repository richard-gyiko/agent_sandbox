"""DSL loading and materialization for AgentSandbox."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import yaml

from agent_sandbox.env import getenv
from agent_sandbox.schema import validate_kind_schema


def labs_v3_root() -> Path:
    configured = getenv("AGENT_SANDBOX_V3_DIR")
    if configured:
        return Path(configured)
    return Path.cwd() / "labs" / "v3"


def environment_root() -> Path:
    return labs_v3_root() / "environments"


def scenario_root() -> Path:
    return labs_v3_root() / "scenarios"


def run_root() -> Path:
    return labs_v3_root() / "runs"


def list_environments(root: Path | None = None) -> list[Path]:
    base = root or environment_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_scenarios(root: Path | None = None) -> list[Path]:
    base = root or scenario_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_runs(root: Path | None = None) -> list[Path]:
    base = root or run_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_run_ids_for_tier(tier: str, root: Path | None = None) -> list[str]:
    normalized = tier.strip().lower()
    if normalized not in {"p0-smoke", "p1-deep"}:
        raise ValueError(f"Unsupported run tier: {tier}")

    run_paths = list_runs(root=root)
    run_ids: list[str] = []
    for run_path in run_paths:
        run_spec = load_run(run_path)
        scenario = load_scenario(resolve_scenario_path(run_spec["scenario_ref"]))
        tags_raw = scenario.get("meta", {}).get("tags", [])
        scenario_tags = {
            str(item).strip().lower()
            for item in tags_raw
            if str(item).strip()
        }
        has_p0 = "p0" in scenario_tags
        determinism = "llm-variant" if "llm-variant" in scenario_tags else "strict"
        include = (
            has_p0 and determinism == "strict"
            if normalized == "p0-smoke"
            else (not has_p0 or determinism == "llm-variant")
        )
        if include:
            run_id = run_spec.get("meta", {}).get("id", run_path.stem)
            run_ids.append(str(run_id))
    return sorted(run_ids)


def _resolve_named_yaml(name_or_path: str, base: Path, kind: str) -> Path:
    candidate = Path(name_or_path)
    if candidate.exists():
        return candidate
    with_ext = base / f"{name_or_path}.yaml"
    if with_ext.exists():
        return with_ext
    raise FileNotFoundError(f"{kind} not found: {name_or_path}")


def resolve_environment_path(name_or_path: str, root: Path | None = None) -> Path:
    return _resolve_named_yaml(name_or_path, root or environment_root(), "Environment")


def resolve_scenario_path(name_or_path: str, root: Path | None = None) -> Path:
    return _resolve_named_yaml(name_or_path, root or scenario_root(), "Scenario")


def resolve_run_path(name_or_path: str, root: Path | None = None) -> Path:
    return _resolve_named_yaml(name_or_path, root or run_root(), "Run")


def _validate_schema(kind: str, data: dict[str, Any]) -> None:
    validate_kind_schema(kind, data)


def _load_v3_doc(path: str | Path) -> dict[str, Any]:
    data = yaml.safe_load(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"Spec must be a mapping: {path}")
    version = data.get("version")
    if version != 3:
        raise ValueError(f"Unsupported spec version: {version} ({path})")
    kind = data.get("kind")
    if kind not in {"environment", "scenario", "run"}:
        raise ValueError(f"Unsupported spec kind '{kind}' ({path})")
    _validate_schema(kind, data)
    return data


def load_environment(path: str | Path) -> dict[str, Any]:
    data = _load_v3_doc(path)
    if data.get("kind") != "environment":
        raise ValueError(f"Expected environment kind: {path}")
    return data


def load_scenario(path: str | Path) -> dict[str, Any]:
    data = _load_v3_doc(path)
    if data.get("kind") != "scenario":
        raise ValueError(f"Expected scenario kind: {path}")
    assertions = data["expect"].get("assertions", [])
    if not isinstance(assertions, list):
        raise ValueError("Scenario expect.assertions must be a list")
    for idx, item in enumerate(assertions):
        if not isinstance(item, dict):
            raise ValueError(f"Assertion at index {idx} must be an object")
        if "kind" not in item:
            raise ValueError(f"Assertion at index {idx} is missing kind")
    return data


def load_run(path: str | Path) -> dict[str, Any]:
    data = _load_v3_doc(path)
    if data.get("kind") != "run":
        raise ValueError(f"Expected run kind: {path}")
    return data


def materialize_run(run_spec: dict[str, Any]) -> dict[str, Any]:
    env_ref = run_spec["environment_ref"]
    scenario_ref = run_spec["scenario_ref"]
    env_spec = load_environment(resolve_environment_path(env_ref))
    scenario_spec = load_scenario(resolve_scenario_path(scenario_ref))

    compiled = json.loads(json.dumps(scenario_spec))
    env_runtime = env_spec.get("runtime", {}).get("env", {})
    scenario_runtime = scenario_spec.get("runtime", {}).get("env", {})
    run_runtime = run_spec.get("runtime", {}).get("env", {})
    merged_runtime_env: dict[str, str] = {}
    for source in (env_runtime, scenario_runtime, run_runtime):
        if isinstance(source, dict):
            for key, value in source.items():
                merged_runtime_env[str(key)] = str(value)
    compiled.setdefault("runtime", {})
    compiled["runtime"]["env"] = merged_runtime_env

    run_block = compiled.setdefault("run", {})
    execution = run_spec.get("execution", {})
    target = execution.get("target")
    if target:
        run_block["target"] = target
    if execution.get("agent_id"):
        run_block["agent_id"] = execution["agent_id"]
    if execution.get("session_state"):
        run_block["session_state"] = execution["session_state"]

    merged_plugins: list[str] = []
    for plugin in env_spec.get("plugins", []):
        merged_plugins.append(str(plugin))
    for plugin in run_spec.get("plugins", []):
        merged_plugins.append(str(plugin))
    if merged_plugins:
        run_block["plugins"] = merged_plugins

    mode_override = run_spec.get("assertions", {}).get("mode")
    if mode_override:
        compiled.setdefault("expect", {})
        compiled["expect"]["mode"] = mode_override

    compiled.setdefault("meta", {})
    compiled["meta"]["run_id"] = run_spec.get("meta", {}).get("id", "")
    compiled["meta"]["environment_id"] = env_spec.get("meta", {}).get("id", "")
    compiled["meta"]["scenario_id"] = scenario_spec.get("meta", {}).get("id", "")
    compiled["__v3_run"] = run_spec
    compiled["__v3_environment"] = env_spec
    return compiled
