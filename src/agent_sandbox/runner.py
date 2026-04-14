"""Scenario runner utilities for twin-backed integration and manual labs usage."""

from __future__ import annotations

import importlib
import json
import subprocess
import time
import urllib.parse
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen

import yaml
from jsonschema import Draft202012Validator, ValidationError

from agent_sandbox import action_handlers, assertion_handlers
from agent_sandbox import adapters as adapter_runtime
from agent_sandbox import execution_registry as registry_runtime
from agent_sandbox import plugins as plugin_runtime
from agent_sandbox.env import getenv, scoped_env
from agent_sandbox.schema import load_schema_json, validate_kind_schema, validate_schema_doc
from agent_sandbox.target_sdk import capture_events
from agent_sandbox.telemetry import observed_span
from agent_sandbox.twin_provider import get_all_twin_providers, list_twin_providers
from agent_sandbox.validation import plugin_policy_env

AssertionHandler = Callable[[dict[str, Any], "ScenarioContext"], None]
ActionHandler = Callable[[dict[str, Any], "ScenarioContext"], None]
WorkflowExecutionHandler = Callable[[dict[str, Any], "TwinEndpoints", str], dict[str, Any]]
AgentExecutionHandler = Callable[[dict[str, Any], "TwinEndpoints"], dict[str, Any]]


def register_assertion(kind: str, handler: AssertionHandler) -> None:
    """Register an assertion handler for scenario expectations."""
    registry_runtime.register_assertion(kind, handler)


def register_action(kind: str, handler: ActionHandler) -> None:
    """Register an action handler for scenario runtime hooks."""
    registry_runtime.register_action(kind, handler)


def register_workflow_runner(target: str, handler: WorkflowExecutionHandler) -> None:
    """Register a workflow execution handler for a run target."""
    normalized = target.strip()
    if not normalized:
        raise ValueError("Workflow runner target cannot be empty")
    registry_runtime.register_workflow_runner(normalized, handler)


def register_agent_runner(
    agent_id: str,
    handler: AgentExecutionHandler,
    aliases: tuple[str, ...] = (),
) -> None:
    """Register an agent execution handler and optional aliases."""
    canonical = _normalize_agent_id(agent_id)
    normalized_aliases = tuple(_normalize_agent_id(alias) for alias in aliases)
    registry_runtime.register_agent_runner(canonical, handler, aliases=normalized_aliases)


def register_http_protocol(name: str, handler: Any) -> None:
    """Register an HTTP adapter protocol handler."""
    adapter_runtime.register_http_protocol(name, handler)


def register_assertion_param_schema(kind: str, schema_relpath: str) -> None:
    """Register JSON schema path for assertion params validation."""
    registry_runtime.register_assertion_param_schema(kind.strip(), schema_relpath.strip())


def register_action_param_schema(kind: str, schema_relpath: str) -> None:
    """Register JSON schema path for action params validation."""
    registry_runtime.register_action_param_schema(kind.strip(), schema_relpath.strip())


def register_http_protocol_config_schema(protocol: str, schema_relpath: str) -> None:
    """Register JSON schema path for HTTP protocol config validation."""
    adapter_runtime.register_http_protocol_config_schema(protocol, schema_relpath)


def _unsafe_plugins_enabled() -> bool:
    return plugin_runtime.unsafe_plugins_enabled()


def _plugin_allowlist() -> set[str]:
    return plugin_runtime.plugin_allowlist()


def load_execution_plugins(module_names: list[str]) -> None:
    """Load plugin modules that self-register execution handlers."""
    plugin_runtime.load_execution_plugins_with_importer(
        module_names,
        import_module_fn=importlib.import_module,
    )


@dataclass
class ScenarioContext:
    """Execution context passed to action and assertion plugins."""

    scenario: dict[str, Any]
    snapshot: dict[str, Any]
    workflow_metrics: dict[str, Any] | None = None
    run_metadata: dict[str, Any] | None = None


@dataclass
class TwinEndpoints:
    """HTTP endpoints for twin services."""

    urls: dict[str, str]

    @property
    def gmail_base_url(self) -> str:
        return self.urls.get("gmail", "http://localhost:9200")

    @gmail_base_url.setter
    def gmail_base_url(self, value: str) -> None:
        self.urls["gmail"] = value

    @property
    def drive_base_url(self) -> str:
        return self.urls.get("drive", "http://localhost:9100")

    @drive_base_url.setter
    def drive_base_url(self, value: str) -> None:
        self.urls["drive"] = value


class TwinUnavailableError(RuntimeError):
    """Raised when twin services are not reachable."""


@dataclass
class ObservabilityStatus:
    """DuckLens availability for labs runs."""

    available: bool
    base_url: str
    reason: str = ""


def default_endpoints() -> TwinEndpoints:
    """Resolve twin endpoints from registered providers and environment."""
    _ensure_twin_providers()
    urls: dict[str, str] = {}
    for name, provider in get_all_twin_providers().items():
        env_var = provider.env_var_name()
        default = provider.default_base_url()
        urls[name] = getenv(env_var, default=default) or default
    return TwinEndpoints(urls=urls)


def ducklens_base_url() -> str:
    """Resolve DuckLens base URL."""
    return (
        getenv(
            "AGENT_SANDBOX_OBSERVABILITY_BASE_URL",
            default="http://localhost:7080",
        )
        or "http://localhost:7080"
    )


def labs_v3_root() -> Path:
    """Resolve v3 root directory used by labs CLI and tests."""
    configured = getenv("AGENT_SANDBOX_V3_DIR")
    if configured:
        return Path(configured)
    return Path.cwd() / "labs" / "v3"


def environment_root() -> Path:
    """Resolve v3 environment directory."""
    return labs_v3_root() / "environments"


def scenario_root() -> Path:
    """Resolve v3 scenario directory."""
    return labs_v3_root() / "scenarios"


def run_root() -> Path:
    """Resolve v3 run directory."""
    return labs_v3_root() / "runs"


def list_environments(root: Path | None = None) -> list[Path]:
    """List available v3 environment YAML files."""
    base = root or environment_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_scenarios(root: Path | None = None) -> list[Path]:
    """List available v3 scenario YAML files."""
    base = root or scenario_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_runs(root: Path | None = None) -> list[Path]:
    """List available v3 run YAML files."""
    base = root or run_root()
    if not base.exists():
        return []
    return sorted(base.glob("*.yaml"))


def list_run_ids_for_tier(tier: str, root: Path | None = None) -> list[str]:
    """Resolve run IDs that belong to a named execution tier.

    Tier rules:
    - p0-smoke: run specs whose linked scenario has both `p0` and `strict` tags
    - p1-deep: all remaining runs, including `llm-variant` scenarios
    """
    normalized = tier.strip().lower()
    if normalized not in {"p0-smoke", "p1-deep"}:
        raise ValueError(f"Unsupported run tier: {tier}")

    run_paths = list_runs(root=root)
    run_ids: list[str] = []
    for run_path in run_paths:
        run_spec = load_run(run_path)
        scenario = load_scenario(resolve_scenario_path(run_spec["scenario_ref"]))
        tags_raw = scenario.get("meta", {}).get("tags", [])
        scenario_tags = {str(item).strip().lower() for item in tags_raw if str(item).strip()}
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
    """Resolve an environment by path or simple name."""
    base = root or environment_root()
    return _resolve_named_yaml(name_or_path, base, "Environment")


def resolve_scenario_path(name_or_path: str, root: Path | None = None) -> Path:
    """Resolve a scenario by path or simple name."""
    base = root or scenario_root()
    return _resolve_named_yaml(name_or_path, base, "Scenario")


def resolve_run_path(name_or_path: str, root: Path | None = None) -> Path:
    """Resolve a run by path or simple name."""
    base = root or run_root()
    return _resolve_named_yaml(name_or_path, base, "Run")


def _post_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    req = Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))


def _request_json(
    url: str,
    payload: dict[str, Any],
    *,
    method: str = "POST",
    headers: dict[str, str] | None = None,
    timeout_s: float | None = None,
) -> dict[str, Any]:
    return adapter_runtime.request_json(
        url,
        payload,
        method=method,
        headers=headers,
        timeout_s=timeout_s,
    )


def _request_form(
    url: str,
    form_data: dict[str, str],
    *,
    headers: dict[str, str] | None = None,
    timeout_s: float | None = None,
) -> dict[str, Any]:
    return adapter_runtime.request_form(
        url,
        form_data,
        headers=headers,
        timeout_s=timeout_s,
    )


def _ensure_http_protocol_registry() -> None:
    adapter_runtime.ensure_http_protocol_registry()


def _get_json(url: str) -> Any:
    req = Request(url, method="GET")
    with urlopen(req) as response:
        return json.loads(response.read().decode("utf-8"))


def _validate_schema(kind: str, data: dict[str, Any]) -> None:
    validate_kind_schema(kind, data)


def _validate_adapter_doc(kind: str, data: dict[str, Any]) -> None:
    validate_schema_doc(f"{kind}.schema.json", data)


def _load_schema_by_relpath(schema_relpath: str) -> dict[str, Any]:
    return load_schema_json(schema_relpath)


def _validate_params_with_schema(
    *,
    schema_relpath: str,
    params: dict[str, Any],
    label: str,
) -> None:
    try:
        schema = _load_schema_by_relpath(schema_relpath)
        Draft202012Validator(schema).validate(params)
    except ValidationError as error:
        message = error.message
        path = ".".join(str(part) for part in error.path)
        if path:
            message = f"{message} (at params.{path})"
        raise ValueError(f"{label} params validation failed: {message}") from error
    except FileNotFoundError as error:
        raise ValueError(f"{label} schema not found: {schema_relpath}") from error


def _validate_scenario_contracts(scenario: dict[str, Any]) -> None:
    assertions = scenario.get("expect", {}).get("assertions", [])
    for index, assertion in enumerate(assertions):
        kind = str(assertion.get("kind", "")).strip()
        if not kind:
            raise ValueError(f"Scenario assertion at index {index} is missing kind")
        if not registry_runtime.has_assertion(kind):
            available = ", ".join(registry_runtime.list_assertion_kinds())
            raise ValueError(f"Unknown scenario assertion kind '{kind}'. Available: {available}")
        schema_relpath = registry_runtime.get_assertion_param_schema(kind)
        if schema_relpath:
            params = assertion.get("params", {})
            if not isinstance(params, dict):
                raise ValueError(
                    f"Assertion '{kind}' params must be an object, got {type(params).__name__}"
                )
            _validate_params_with_schema(
                schema_relpath=schema_relpath,
                params=params,
                label=f"Assertion '{kind}'",
            )

    actions = scenario.get("actions", [])
    for index, action in enumerate(actions):
        kind = str(action.get("kind", "")).strip()
        if not kind:
            raise ValueError(f"Scenario action at index {index} is missing kind")
        if not registry_runtime.has_action(kind):
            available = ", ".join(registry_runtime.list_action_kinds())
            raise ValueError(f"Unknown scenario action kind '{kind}'. Available: {available}")
        schema_relpath = registry_runtime.get_action_param_schema(kind)
        if schema_relpath:
            params = action.get("params", {})
            if not isinstance(params, dict):
                raise ValueError(
                    f"Action '{kind}' params must be an object, got {type(params).__name__}"
                )
            _validate_params_with_schema(
                schema_relpath=schema_relpath,
                params=params,
                label=f"Action '{kind}'",
            )


def ensure_twins_available(endpoints: TwinEndpoints, timeout_s: float = 20.0) -> None:
    """Validate that all registered twin services are reachable."""
    _ensure_twin_providers()
    providers = get_all_twin_providers()
    if not providers:
        raise TwinUnavailableError("No twin providers registered.")
    deadline = time.time() + timeout_s
    last_error: Exception | None = None
    while time.time() < deadline:
        try:
            for name, provider in providers.items():
                base_url = endpoints.urls.get(name, provider.default_base_url())
                provider.health_check(base_url)
            return
        except Exception as error:
            last_error = error
            time.sleep(0.25)
    raise TwinUnavailableError(
        "Twin environment is not reachable. Start it with `agent-sandbox env up`."
    ) from last_error


def get_observability_status() -> ObservabilityStatus:
    """Check whether DuckLens API is reachable."""
    base_url = ducklens_base_url().rstrip("/")
    try:
        _get_json(f"{base_url}/api/overview")
        return ObservabilityStatus(available=True, base_url=base_url)
    except Exception as error:
        return ObservabilityStatus(
            available=False,
            base_url=base_url,
            reason=str(error),
        )


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
    """Load and validate a DSL v3 environment file."""
    data = _load_v3_doc(path)
    if data.get("kind") != "environment":
        raise ValueError(f"Expected environment kind: {path}")
    return data


def load_scenario(path: str | Path) -> dict[str, Any]:
    """Load and validate a DSL v3 scenario file."""
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
    """Load and validate a DSL v3 run file."""
    data = _load_v3_doc(path)
    if data.get("kind") != "run":
        raise ValueError(f"Expected run kind: {path}")
    return data


def materialize_run(run_spec: dict[str, Any]) -> dict[str, Any]:
    """Materialize a v3 run file into a compiled executable scenario."""
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


def reset_twins(endpoints: TwinEndpoints) -> None:
    """Reset all registered twin services to empty state."""
    _ensure_twin_providers()
    for name, provider in get_all_twin_providers().items():
        base_url = endpoints.urls.get(name, provider.default_base_url())
        provider.reset(base_url)


def seed_twins(endpoints: TwinEndpoints, scenario: dict[str, Any]) -> None:
    """Seed twin states from scenario definition."""
    _ensure_twin_providers()
    seed = scenario.get("seed", {})
    for name, provider in get_all_twin_providers().items():
        if name in seed:
            base_url = endpoints.urls.get(name, provider.default_base_url())
            provider.seed(base_url, seed[name])


def snapshot_twins(endpoints: TwinEndpoints) -> dict[str, Any]:
    """Collect complete snapshot and operation logs from all registered twins."""
    _ensure_twin_providers()
    ensure_twins_available(endpoints)
    result: dict[str, Any] = {}
    for name, provider in get_all_twin_providers().items():
        base_url = endpoints.urls.get(name, provider.default_base_url())
        result[name] = provider.snapshot(base_url)
        result[f"{name}_ops"] = provider.events(base_url)
    return result


def run_scenario_actions(
    scenario: dict[str, Any],
    snapshot: dict[str, Any],
    workflow_metrics: dict[str, Any] | None = None,
    run_metadata: dict[str, Any] | None = None,
) -> None:
    """Execute action hooks declared by the scenario."""
    actions = scenario.get("actions", [])
    context = ScenarioContext(
        scenario=scenario,
        snapshot=snapshot,
        workflow_metrics=workflow_metrics,
        run_metadata=run_metadata,
    )
    for action in actions:
        kind = action.get("kind")
        if not kind:
            raise ValueError("Scenario action missing kind")
        handler = registry_runtime.get_action(kind)
        if handler is None:
            raise ValueError(f"Unknown scenario action kind: {kind}")
        handler(action, context)


def _extract_span_attributes(raw: Any) -> dict[str, Any]:
    if isinstance(raw, dict):
        return raw
    if isinstance(raw, str) and raw.strip():
        try:
            parsed = json.loads(raw)
            if isinstance(parsed, dict):
                return parsed
        except Exception:
            return {}
    return {}


def _query_ducklens_spans_for_run(run_id: str, limit_traces: int = 100) -> list[dict[str, Any]]:
    """Fetch spans from DuckLens and keep only those for a specific labs run id."""
    base = ducklens_base_url().rstrip("/")
    traces = _get_json(f"{base}/api/traces?limit={limit_traces}")
    if not isinstance(traces, list):
        return []

    spans_for_run: list[dict[str, Any]] = []
    for trace in traces:
        trace_id = trace.get("trace_id")
        if not trace_id:
            continue
        detail = _get_json(f"{base}/api/traces/{trace_id}")
        for span in detail.get("spans", []):
            attributes = _extract_span_attributes(span.get("attributes"))
            if attributes.get("agent_sandbox.run.id") != run_id:
                continue
            spans_for_run.append(
                {
                    "trace_id": trace_id,
                    "span_name": span.get("span_name"),
                    "attributes": attributes,
                }
            )
    return spans_for_run


def _resolve_trace_assertion_run_id(assertion: dict[str, Any], context: ScenarioContext) -> str:
    params = assertion.get("params", {})
    if "run_id" in params:
        return str(params["run_id"])
    if context.run_metadata and context.run_metadata.get("run_id"):
        return str(context.run_metadata["run_id"])
    raise AssertionError("Trace assertion requires run_id (params.run_id or runtime run metadata)")


def _assert_trace_has_span(assertion: dict[str, Any], context: ScenarioContext) -> None:
    params = assertion.get("params", {})
    run_id = _resolve_trace_assertion_run_id(assertion, context)
    span_name = params["span_name"]
    min_count = int(params.get("min_count", 1))
    timeout_s = float(params.get("timeout_s", 30.0))
    matched: list[dict[str, Any]] = []
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        spans = _query_ducklens_spans_for_run(run_id, limit_traces=500)
        matched = [span for span in spans if span.get("span_name") == span_name]
        if len(matched) >= min_count:
            break
        time.sleep(0.5)
    assert len(matched) >= min_count, (
        f"Expected at least {min_count} spans named '{span_name}' for run_id={run_id}, "
        f"got {len(matched)}"
    )


def _assert_trace_span_attr_equals(assertion: dict[str, Any], context: ScenarioContext) -> None:
    params = assertion.get("params", {})
    run_id = _resolve_trace_assertion_run_id(assertion, context)
    attr_key = params["key"]
    expected_value = params["value"]
    span_name = params.get("span_name")
    min_count = int(params.get("min_count", 1))
    timeout_s = float(params.get("timeout_s", 30.0))
    matched = 0
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        spans = _query_ducklens_spans_for_run(run_id, limit_traces=500)
        matched = 0
        for span in spans:
            if span_name and span.get("span_name") != span_name:
                continue
            attributes = span.get("attributes", {})
            if attributes.get(attr_key) == expected_value:
                matched += 1
        if matched >= min_count:
            break
        time.sleep(0.5)

    assert matched >= min_count, (
        f"Expected at least {min_count} span(s) "
        f"with {attr_key}={expected_value} for run_id={run_id}, got {matched}"
    )


def _register_defaults() -> None:
    assertion_handlers.register_default_assertions(
        register_assertion=register_assertion,
        register_assertion_param_schema=register_assertion_param_schema,
    )
    register_assertion("trace.has_span", _assert_trace_has_span)
    register_assertion_param_schema(
        "trace.has_span",
        "assertions/trace.has_span.params.schema.json",
    )
    register_assertion("trace.span_attr_equals", _assert_trace_span_attr_equals)
    register_assertion_param_schema(
        "trace.span_attr_equals",
        "assertions/trace.span_attr_equals.params.schema.json",
    )
    action_handlers.register_default_actions(
        register_action=register_action,
        register_action_param_schema=register_action_param_schema,
    )


_register_defaults()


def assert_scenario_expectations(
    scenario: dict[str, Any],
    snapshot: dict[str, Any],
    workflow_metrics: dict[str, Any] | None = None,
    run_metadata: dict[str, Any] | None = None,
) -> None:
    """Evaluate all scenario assertions via registered plugins."""
    context = ScenarioContext(
        scenario=scenario,
        snapshot=snapshot,
        workflow_metrics=workflow_metrics,
        run_metadata=run_metadata,
    )
    mode = scenario.get("expect", {}).get("mode", "strict")
    assertions = scenario.get("expect", {}).get("assertions", [])
    failures: list[str] = []
    for assertion in assertions:
        kind = assertion.get("kind")
        if not kind:
            raise ValueError("Scenario assertion missing kind")
        handler = registry_runtime.get_assertion(kind)
        if handler is None:
            raise ValueError(f"Unknown scenario assertion kind: {kind}")
        if mode == "soft":
            try:
                handler(assertion, context)
            except AssertionError as error:
                failures.append(f"{kind}: {error}")
        else:
            handler(assertion, context)
    if failures:
        details = "; ".join(failures)
        raise AssertionError(f"Scenario assertions failed ({len(failures)}): {details}")


def run_env_up(compose_file: str = "docker-compose.twins.yml") -> int:
    """Start twin environment via docker compose."""
    return subprocess.call(["docker", "compose", "-f", compose_file, "up", "-d", "--build"])


def run_env_down(compose_file: str = "docker-compose.twins.yml", purge: bool = True) -> int:
    """Stop twin environment and optionally purge volumes."""
    args = ["docker", "compose", "-f", compose_file, "down"]
    if purge:
        args.append("-v")
    return subprocess.call(args)


def execute_run_spec(
    run_spec: dict[str, Any],
    endpoints: TwinEndpoints,
    session_id: str,
) -> dict[str, Any]:
    """Execute compiled run spec by its execution mode."""
    execution = run_spec.get("execution", {})
    adapter = execution.get("adapter")
    if isinstance(adapter, dict):
        compiled = materialize_run(run_spec)
        return _execute_adapter_run(compiled, run_spec, endpoints, session_id)

    mode = execution.get("mode")
    if mode == "workflow":
        compiled = materialize_run(run_spec)
        return run_workflow_for_scenario(compiled, endpoints=endpoints, session_id=session_id)
    if mode == "agent":
        compiled = materialize_run(run_spec)
        agent_id = compiled.get("run", {}).get("agent_id", "")
        if not agent_id:
            raise ValueError("Run execution.agent_id is required for agent mode")
        return run_agent_for_scenario(compiled, endpoints=endpoints, agent_id=agent_id)
    raise ValueError(f"Unsupported run execution mode: {mode}")


def list_registered_targets(kind: str | None = None) -> list[str]:
    """List available registered execution targets."""
    _ensure_execution_registry()
    workflow_targets = registry_runtime.list_workflow_targets()
    agent_ids = registry_runtime.list_agent_canonical_ids()
    if kind == "workflow":
        return [f"workflow:{item}" for item in workflow_targets]
    if kind == "agent":
        return [f"agent:{item}" for item in agent_ids]
    return sorted(
        [
            *(f"workflow:{item}" for item in workflow_targets),
            *(f"agent:{item}" for item in agent_ids),
        ]
    )


def list_capabilities() -> dict[str, Any]:
    """List available labs capabilities for validation and execution."""
    _ensure_execution_registry()
    return {
        "adapters": {
            "types": ["command", "http"],
            "http_protocols": adapter_runtime.list_http_protocols(),
        },
        "assertions": registry_runtime.list_assertion_kinds(),
        "actions": registry_runtime.list_action_kinds(),
        "validation": {
            "assertion_param_schemas": registry_runtime.list_assertion_param_schema_kinds(),
            "action_param_schemas": registry_runtime.list_action_param_schema_kinds(),
            "http_protocol_config_schemas": adapter_runtime.list_http_protocol_config_schemas(),
        },
        "targets": {
            "workflow": registry_runtime.list_workflow_targets(),
            "agent": registry_runtime.list_agent_canonical_ids(),
        },
        "plugins": {
            "loaded_modules": plugin_runtime.list_loaded_modules(),
            "unsafe_enabled": _unsafe_plugins_enabled(),
            "allowlist": sorted(_plugin_allowlist()),
        },
    }


def validate_run_spec(run_spec: dict[str, Any]) -> dict[str, Any]:
    """Validate run spec references, plugins, and execution targets."""
    _ensure_execution_registry()
    env_spec = load_environment(resolve_environment_path(run_spec["environment_ref"]))
    scenario = load_scenario(resolve_scenario_path(run_spec["scenario_ref"]))

    env_plugins = [str(item) for item in env_spec.get("plugins", []) if str(item).strip()]
    run_plugins = [str(item) for item in run_spec.get("plugins", []) if str(item).strip()]
    env_runtime = env_spec.get("runtime", {}).get("env", {})
    env_overrides = {
        str(key): str(value)
        for key, value in env_runtime.items()
        if str(key).startswith("AGENT_SANDBOX_")
    }
    env_overrides = {**plugin_policy_env(env_spec), **env_overrides}
    with scoped_env(overrides=env_overrides):
        load_execution_plugins([*env_plugins, *run_plugins])
    _validate_scenario_contracts(scenario)

    execution = run_spec.get("execution", {})
    mode = execution.get("mode")
    isolation = str(execution.get("isolation", "sandbox_only")).strip().lower()
    if isolation not in {"sandbox_only", "allow_live"}:
        raise ValueError(f"Unsupported run execution.isolation: {isolation}")
    target = execution.get("target", "")
    agent_id = execution.get("agent_id", "")
    adapter = execution.get("adapter")
    if isinstance(adapter, dict):
        if not target:
            raise ValueError("Run execution.target is required when execution.adapter is set")
        adapter_type = str(adapter.get("type", "")).strip().lower()
        adapter_protocol = ""
        if adapter_type not in {"command", "http"}:
            raise ValueError(f"Unsupported execution.adapter.type: {adapter_type}")
        if adapter_type == "http":
            adapter_config = adapter.get("config", {})
            if not isinstance(adapter_config, dict):
                raise ValueError("Run execution.adapter.config must be an object")
            adapter_protocol = str(adapter_config.get("protocol", "generic-json")).strip().lower()
            if not adapter_runtime.has_http_protocol(adapter_protocol):
                available = ", ".join(adapter_runtime.list_http_protocols())
                raise ValueError(
                    f"Unsupported execution.adapter.config.protocol: {adapter_protocol}. "
                    f"Available: {available}"
                )
            schema_relpath = adapter_runtime.get_http_protocol_config_schema(adapter_protocol)
            if schema_relpath:
                _validate_params_with_schema(
                    schema_relpath=schema_relpath,
                    params=adapter_config,
                    label=f"HTTP protocol '{adapter_protocol}' config",
                )
            if isolation == "sandbox_only":
                _validate_sandbox_only_http_adapter(
                    adapter_protocol=adapter_protocol,
                    adapter_config=adapter_config,
                )
        return {
            "valid": True,
            "run_id": run_spec.get("meta", {}).get("id", ""),
            "mode": mode,
            "isolation": isolation,
            "target": target,
            "agent_id": agent_id,
            "adapter_type": adapter_type,
            "adapter_protocol": adapter_protocol,
            "environment_ref": run_spec.get("environment_ref", ""),
            "scenario_ref": run_spec.get("scenario_ref", ""),
        }

    if mode == "workflow":
        if not target:
            raise ValueError("Run execution.target is required for workflow mode")
        if not registry_runtime.has_workflow_target(target):
            available = ", ".join(registry_runtime.list_workflow_targets())
            raise ValueError(f"Unknown workflow target '{target}'. Available: {available}")
    elif mode == "agent":
        if not agent_id:
            raise ValueError("Run execution.agent_id is required for agent mode")
        normalized_agent = _normalize_agent_id(agent_id)
        if not registry_runtime.has_agent_runner(normalized_agent):
            available = ", ".join(registry_runtime.list_agent_canonical_ids())
            raise ValueError(f"Unknown agent id '{agent_id}'. Available: {available}")
        if target and not target.startswith("agent."):
            raise ValueError(f"Agent run execution.target must start with 'agent.': {target}")
    else:
        raise ValueError(f"Unsupported run execution mode: {mode}")

    return {
        "valid": True,
        "run_id": run_spec.get("meta", {}).get("id", ""),
        "mode": mode,
        "isolation": isolation,
        "target": target,
        "agent_id": agent_id,
        "environment_ref": run_spec.get("environment_ref", ""),
        "scenario_ref": run_spec.get("scenario_ref", ""),
    }


def _sandbox_http_allowed_hosts() -> set[str]:
    raw = getenv("AGENT_SANDBOX_SANDBOX_HTTP_HOSTS", default="") or ""
    if not raw.strip():
        return {"localhost", "127.0.0.1", "::1", "host.docker.internal"}
    return {item.strip().lower() for item in raw.split(",") if item.strip()}


def _validate_sandbox_only_http_adapter(
    *,
    adapter_protocol: str,
    adapter_config: dict[str, Any],
) -> None:
    if adapter_protocol == "agno-agentos-workflow":
        raise ValueError(
            "execution.adapter.config.protocol=agno-agentos-workflow is blocked in "
            "sandbox_only mode. Set execution.isolation=allow_live to opt in."
        )
    url = str(adapter_config.get("url", "")).strip()
    parsed = urllib.parse.urlparse(url)
    host = (parsed.hostname or "").strip().lower()
    allowed_hosts = _sandbox_http_allowed_hosts()
    if not host or host not in allowed_hosts:
        allowed = ", ".join(sorted(allowed_hosts))
        raise ValueError(
            "execution.adapter.config.url host is not allowed in sandbox_only mode: "
            f"{host or '(empty)'}. Allowed: {allowed}"
        )


def _execute_adapter_run(
    compiled: dict[str, Any],
    run_spec: dict[str, Any],
    endpoints: TwinEndpoints,
    session_id: str,
) -> dict[str, Any]:
    return adapter_runtime.execute_adapter_run(
        compiled,
        run_spec,
        endpoints,
        session_id,
        validate_adapter_doc=_validate_adapter_doc,
        request_json_fn=_request_json,
        request_form_fn=_request_form,
    )


def run_workflow_for_scenario(
    scenario: dict[str, Any],
    endpoints: TwinEndpoints,
    session_id: str = "agent-sandbox-session",
) -> dict[str, Any]:
    """Execute a scenario workflow by registered run target."""
    _ensure_execution_registry()
    _load_run_plugins(scenario)
    run = scenario.get("run", {})
    target = run.get("target", "workflow.email_document_processor")
    handler = registry_runtime.get_workflow_runner(target)
    if handler is None:
        available = ", ".join(registry_runtime.list_workflow_targets())
        raise ValueError(f"Unsupported run target: {target}. Available: {available}")

    with (
        _apply_runtime_env(scenario, endpoints),
        observed_span(
            "agent_sandbox.workflow.run",
            attributes={
                "agent_sandbox.run.mode": "workflow",
                "agent_sandbox.run.id": session_id,
                "agent_sandbox.run.target": target,
                "agent_sandbox.scenario.name": scenario.get("meta", {}).get("name", ""),
            },
        ),
    ):
        with capture_events() as runtime_events:
            result = handler(scenario, endpoints, session_id)
        return _attach_runtime_events(result, runtime_events)


def run_agent_for_scenario(
    scenario: dict[str, Any],
    endpoints: TwinEndpoints,
    agent_id: str,
) -> dict[str, Any]:
    """Execute a scenario agent by registered agent ID."""
    _ensure_execution_registry()
    _load_run_plugins(scenario)
    normalized = _normalize_agent_id(agent_id)
    handler = registry_runtime.get_agent_runner(normalized)
    if handler is None:
        available = ", ".join(registry_runtime.list_agent_canonical_ids())
        raise ValueError(f"Unsupported agent id: {agent_id}. Available: {available}")
    canonical_id = registry_runtime.get_agent_canonical(normalized)
    with (
        _apply_runtime_env(scenario, endpoints),
        observed_span(
            "agent_sandbox.agent.run",
            attributes={
                "agent_sandbox.run.mode": "agent",
                "agent_sandbox.agent.id": canonical_id,
                "agent_sandbox.scenario.name": scenario.get("meta", {}).get("name", ""),
            },
        ),
    ):
        with capture_events() as runtime_events:
            result = handler(scenario, endpoints)
        return _attach_runtime_events(result, runtime_events)


def _normalize_agent_id(agent_id: str) -> str:
    return agent_id.strip().lower().replace("_", "-")


def _apply_runtime_env(scenario: dict[str, Any], endpoints: TwinEndpoints):
    runtime = scenario.get("runtime", {})
    runtime_env = runtime.get("env", {})
    overrides = {str(key): str(value) for key, value in runtime_env.items()}
    runtime_controls: dict[str, str] = {}
    fixed_now = runtime.get("clock", {}).get("fixed_now")
    if fixed_now is not None and str(fixed_now).strip():
        runtime_controls["AGENT_SANDBOX_CLOCK_FIXED_NOW"] = str(fixed_now)
    random_seed = runtime.get("random", {}).get("seed")
    if random_seed is not None and str(random_seed).strip():
        runtime_controls["AGENT_SANDBOX_RANDOM_SEED"] = str(random_seed)
    fault_preset = runtime.get("faults", {}).get("preset")
    if fault_preset is not None and str(fault_preset).strip():
        runtime_controls["AGENT_SANDBOX_FAULT_PRESET"] = str(fault_preset)
    defaults: dict[str, str] = {"AGENT_SANDBOX_RUNTIME_MODE": "twin", "DATABASE_URL": ""}
    for name, provider in get_all_twin_providers().items():
        base_url = endpoints.urls.get(name, provider.default_base_url())
        defaults.update(provider.runtime_env_defaults(base_url, scenario))
    defaults.update(runtime_controls)
    return scoped_env(overrides=overrides, defaults=defaults)


def _attach_runtime_events(
    result: dict[str, Any],
    runtime_events: list[dict[str, Any]],
) -> dict[str, Any]:
    if not runtime_events:
        return result
    session_state = result.get("session_state")
    if isinstance(session_state, dict):
        session_state["agent_sandbox_events"] = runtime_events
        return result
    result["session_state"] = {"agent_sandbox_events": runtime_events}
    return result


def _load_run_plugins(scenario: dict[str, Any]) -> None:
    plugins = scenario.get("run", {}).get("plugins", [])
    if isinstance(plugins, list):
        load_execution_plugins([str(item) for item in plugins if str(item).strip()])


def _ensure_twin_providers() -> None:
    """Register built-in twin providers if none are registered."""
    if list_twin_providers():
        return
    try:
        import agent_sandbox_twins  # noqa: F401
    except ImportError:
        from agent_sandbox._builtin_twins import register_builtin_providers

        register_builtin_providers()


def _ensure_execution_registry() -> None:
    if registry_runtime.is_ready():
        return
    plugin_modules = (
        getenv(
            "AGENT_SANDBOX_PLUGIN_MODULES",
            default="",
        )
        or ""
    ).strip()
    if plugin_modules:
        load_execution_plugins([item for item in plugin_modules.split(",") if item.strip()])
    registry_runtime.set_ready(True)
