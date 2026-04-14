"""Execution adapters and protocol registry for AgentSandbox."""

from __future__ import annotations

import json
import os
import subprocess
import urllib.parse
from collections.abc import Callable
from typing import Any
from urllib.request import Request, urlopen

from agent_sandbox.env import getenv, is_truthy
from agent_sandbox.schema import validate_schema_doc

RequestJsonFn = Callable[..., dict[str, Any]]
RequestFormFn = Callable[..., dict[str, Any]]
ValidateAdapterDocFn = Callable[[str, dict[str, Any]], None]

HttpProtocolHandler = Callable[
    [
        str,
        dict[str, Any],
        dict[str, Any],
        dict[str, str],
        float | None,
        RequestJsonFn,
        RequestFormFn,
    ],
    dict[str, Any],
]

_HTTP_PROTOCOL_HANDLERS: dict[str, HttpProtocolHandler] = {}
_HTTP_PROTOCOL_CONFIG_SCHEMAS: dict[str, str] = {}


def request_json(
    url: str,
    payload: dict[str, Any],
    *,
    method: str = "POST",
    headers: dict[str, str] | None = None,
    timeout_s: float | None = None,
) -> dict[str, Any]:
    request_headers = {"Content-Type": "application/json"}
    if headers:
        request_headers.update(headers)
    req = Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers=request_headers,
        method=method,
    )
    with urlopen(req, timeout=timeout_s) as response:
        return json.loads(response.read().decode("utf-8"))


def request_form(
    url: str,
    form_data: dict[str, str],
    *,
    headers: dict[str, str] | None = None,
    timeout_s: float | None = None,
) -> dict[str, Any]:
    request_headers = {"Content-Type": "application/x-www-form-urlencoded"}
    if headers:
        request_headers.update(headers)
    encoded = urllib.parse.urlencode(form_data).encode("utf-8")
    req = Request(
        url,
        data=encoded,
        headers=request_headers,
        method="POST",
    )
    with urlopen(req, timeout=timeout_s) as response:
        return json.loads(response.read().decode("utf-8"))


def register_http_protocol(name: str, handler: HttpProtocolHandler) -> None:
    normalized = name.strip().lower()
    if not normalized:
        raise ValueError("HTTP protocol name cannot be empty")
    _HTTP_PROTOCOL_HANDLERS[normalized] = handler


def register_http_protocol_config_schema(protocol: str, schema_relpath: str) -> None:
    _HTTP_PROTOCOL_CONFIG_SCHEMAS[protocol.strip().lower()] = schema_relpath.strip()


def list_http_protocols() -> list[str]:
    ensure_http_protocol_registry()
    return sorted(_HTTP_PROTOCOL_HANDLERS.keys())


def list_http_protocol_config_schemas() -> list[str]:
    ensure_http_protocol_registry()
    return sorted(_HTTP_PROTOCOL_CONFIG_SCHEMAS.keys())


def get_http_protocol_config_schema(protocol: str) -> str:
    ensure_http_protocol_registry()
    return _HTTP_PROTOCOL_CONFIG_SCHEMAS.get(protocol.strip().lower(), "")


def has_http_protocol(protocol: str) -> bool:
    ensure_http_protocol_registry()
    return protocol.strip().lower() in _HTTP_PROTOCOL_HANDLERS


def ensure_http_protocol_registry() -> None:
    if _HTTP_PROTOCOL_HANDLERS:
        return
    register_http_protocol("generic-json", _http_protocol_generic_json)
    register_http_protocol("agno-agentos-workflow", _http_protocol_agno_agentos_workflow)
    register_http_protocol_config_schema(
        "generic-json",
        "http_protocols/generic-json.config.schema.json",
    )
    register_http_protocol_config_schema(
        "agno-agentos-workflow",
        "http_protocols/agno-agentos-workflow.config.schema.json",
    )


def _http_protocol_generic_json(
    _protocol: str,
    payload: dict[str, Any],
    config: dict[str, Any],
    request_headers: dict[str, str],
    timeout_s: float | None,
    request_json_fn: RequestJsonFn,
    _request_form_fn: RequestFormFn,
) -> dict[str, Any]:
    url = str(config.get("url", "")).strip()
    method = str(config.get("method", "POST")).upper()
    return request_json_fn(
        url,
        payload,
        method=method,
        headers=request_headers,
        timeout_s=timeout_s,
    )


def _http_protocol_agno_agentos_workflow(
    _protocol: str,
    payload: dict[str, Any],
    config: dict[str, Any],
    request_headers: dict[str, str],
    timeout_s: float | None,
    _request_json_fn: RequestJsonFn,
    request_form_fn: RequestFormFn,
) -> dict[str, Any]:
    url = str(config.get("url", "")).strip()
    form_fields = {
        "message": str(payload.get("input", {}).get("text", "")),
        "stream": "false",
        "session_id": str(payload.get("run", {}).get("id", "")),
        "user_id": "labs",
    }
    custom_form_fields = config.get("form_fields", {})
    if isinstance(custom_form_fields, dict):
        for key, value in custom_form_fields.items():
            form_fields[str(key)] = str(value)
    return request_form_fn(
        url,
        form_fields,
        headers=request_headers,
        timeout_s=timeout_s,
    )


def execute_adapter_run(
    compiled: dict[str, Any],
    run_spec: dict[str, Any],
    endpoints: Any,
    session_id: str,
    *,
    validate_adapter_doc: ValidateAdapterDocFn | None = None,
    request_json_fn: RequestJsonFn | None = None,
    request_form_fn: RequestFormFn | None = None,
) -> dict[str, Any]:
    validate = validate_adapter_doc or _validate_adapter_doc
    request_json_impl = request_json_fn or request_json
    request_form_impl = request_form_fn or request_form

    execution = run_spec.get("execution", {})
    adapter = execution.get("adapter", {})
    adapter_type = str(adapter.get("type", "")).strip().lower()
    retries = int(adapter.get("retries", 0))
    timeout_s = float(adapter.get("timeout_s", 300))
    strict_contract = _resolve_adapter_strict_contract(adapter)
    isolation_mode = _resolve_isolation_mode(run_spec)
    runtime_env = _adapter_runtime_env(compiled, endpoints)
    payload = _build_adapter_input_payload(compiled, run_spec, session_id, runtime_env)
    validate("adapter-input", payload)

    if adapter_type == "command":
        output = _execute_adapter_command(
            payload,
            runtime_env,
            adapter,
            retries,
            timeout_s,
            validate_adapter_doc=validate,
        )
    elif adapter_type == "http":
        output = _execute_adapter_http(
            payload,
            adapter,
            retries,
            timeout_s,
            strict_contract=strict_contract,
            isolation_mode=isolation_mode,
            validate_adapter_doc=validate,
            request_json_fn=request_json_impl,
            request_form_fn=request_form_impl,
        )
    else:
        raise ValueError(f"Unsupported execution.adapter.type: {adapter_type}")
    return _coerce_adapter_result(output, session_id, adapter_type)



def _adapter_runtime_env(
    compiled: dict[str, Any],
    endpoints: Any,
) -> dict[str, str]:
    from agent_sandbox.twin_provider import get_all_twin_providers

    runtime_env = {
        str(key): str(value) for key, value in compiled.get("runtime", {}).get("env", {}).items()
    }
    runtime_env.setdefault("AGENT_SANDBOX_RUNTIME_MODE", "twin")
    for name, provider in get_all_twin_providers().items():
        base_url = getattr(endpoints, "urls", {}).get(name, provider.default_base_url())
        for env_key, env_val in provider.runtime_env_defaults(base_url, compiled).items():
            runtime_env.setdefault(env_key, env_val)
    runtime_env.setdefault("DATABASE_URL", "")
    return runtime_env


def _build_adapter_input_payload(
    compiled: dict[str, Any],
    run_spec: dict[str, Any],
    session_id: str,
    runtime_env: dict[str, str],
) -> dict[str, Any]:
    execution = run_spec.get("execution", {})
    adapter_config = execution.get("adapter", {}).get("config", {})
    scenario_id = str(compiled.get("meta", {}).get("scenario_id", "")).strip()
    default_message = f"Run lab scenario '{scenario_id}'".strip()
    if not scenario_id:
        default_message = "Run lab scenario"
    input_text = default_message
    configured_message = adapter_config.get("message")
    if configured_message is not None:
        configured_message_text = str(configured_message).strip()
        if configured_message_text:
            input_text = configured_message_text
    return {
        "schema_version": "lab.adapter.v1",
        "run": {
            "id": session_id,
            "mode": execution.get("mode", ""),
            "target": execution.get("target", ""),
        },
        "scenario": {
            "id": compiled.get("meta", {}).get("scenario_id", ""),
            "name": compiled.get("meta", {}).get("name", ""),
        },
        "runtime": {
            "env": runtime_env,
            "session_state": execution.get("session_state", {}),
        },
        "input": {"text": input_text},
    }


def _coerce_adapter_result(
    output: dict[str, Any],
    session_id: str,
    adapter_type: str,
) -> dict[str, Any]:
    status = output.get("status", "FAILED")
    result = output.get("result", {})
    metrics = output.get("metrics", {})
    out = {
        "status": status,
        "session_id": session_id,
        "session_state": metrics if isinstance(metrics, dict) else {},
        "content": result.get("content", "") if isinstance(result, dict) else "",
        "result": result if isinstance(result, dict) else {},
        "metrics": metrics if isinstance(metrics, dict) else {},
        "artifacts": output.get("artifacts", []),
        "error": output.get("error"),
        "adapter": adapter_type,
    }
    if status != "COMPLETED":
        message = ""
        error = output.get("error")
        if isinstance(error, dict):
            message = str(error.get("message", ""))
        raise RuntimeError(f"Adapter execution failed with status={status}: {message}".strip())
    return out


def _execute_adapter_command(
    payload: dict[str, Any],
    runtime_env: dict[str, str],
    adapter: dict[str, Any],
    retries: int,
    timeout_s: float,
    *,
    validate_adapter_doc: ValidateAdapterDocFn,
) -> dict[str, Any]:
    config = adapter.get("config", {})
    cmd = config.get("cmd", [])
    if not isinstance(cmd, list) or not cmd:
        raise ValueError(
            "execution.adapter.config.cmd must be a non-empty array for command adapter"
        )
    cwd = config.get("cwd")
    pass_env = bool(config.get("pass_env", True))

    last_error: Exception | None = None
    for _attempt in range(retries + 1):
        try:
            process_env = os.environ.copy() if pass_env else {}
            process_env.update(runtime_env)
            result = subprocess.run(
                [str(item) for item in cmd],
                input=json.dumps(payload),
                capture_output=True,
                text=True,
                timeout=timeout_s if timeout_s > 0 else None,
                cwd=str(cwd) if cwd else None,
                env=process_env,
                check=False,
            )
            if result.returncode != 0:
                raise RuntimeError(
                    f"Adapter command exited with code {result.returncode}: {result.stderr.strip()}"
                )
            output = json.loads(result.stdout or "{}")
            if not isinstance(output, dict):
                raise ValueError("Adapter command output must be a JSON object")
            validate_adapter_doc("adapter-output", output)
            return output
        except Exception as error:
            last_error = error
    raise RuntimeError(f"Adapter command execution failed: {last_error}") from last_error


def _execute_adapter_http(
    payload: dict[str, Any],
    adapter: dict[str, Any],
    retries: int,
    timeout_s: float,
    *,
    strict_contract: bool,
    isolation_mode: str,
    validate_adapter_doc: ValidateAdapterDocFn,
    request_json_fn: RequestJsonFn,
    request_form_fn: RequestFormFn,
) -> dict[str, Any]:
    config = adapter.get("config", {})
    url = str(config.get("url", "")).strip()
    if not url:
        raise ValueError("execution.adapter.config.url is required for http adapter")
    headers = config.get("headers", {})
    protocol = str(config.get("protocol", "generic-json")).strip().lower()
    if not isinstance(headers, dict):
        raise ValueError("execution.adapter.config.headers must be an object")
    _enforce_http_isolation(
        isolation_mode=isolation_mode,
        protocol=protocol,
        url=url,
    )
    ensure_http_protocol_registry()
    protocol_handler = _HTTP_PROTOCOL_HANDLERS.get(protocol)
    if protocol_handler is None:
        available = ", ".join(sorted(_HTTP_PROTOCOL_HANDLERS))
        raise ValueError(
            f"Unsupported execution.adapter.config.protocol: {protocol}. Available: {available}"
        )

    last_error: Exception | None = None
    for _attempt in range(retries + 1):
        try:
            timeout = timeout_s if timeout_s > 0 else None
            request_headers = {str(k): str(v) for k, v in headers.items()}
            output = protocol_handler(
                protocol,
                payload,
                config,
                request_headers,
                timeout,
                request_json_fn,
                request_form_fn,
            )
            if not isinstance(output, dict):
                raise ValueError("Adapter HTTP response must be a JSON object")
            if output.get("schema_version") == "lab.adapter.v1":
                validate_adapter_doc("adapter-output", output)
                return output
            if strict_contract:
                raise ValueError(
                    "Adapter HTTP response must be lab.adapter.v1 "
                    "when strict contract mode is enabled"
                )
            if protocol == "agno-agentos-workflow":
                if "status" not in output:
                    raise ValueError(
                        "Adapter HTTP response for agno-agentos-workflow must include status"
                    )
                normalized = {
                    "schema_version": "lab.adapter.v1",
                    "status": str(output["status"]),
                    "result": {
                        "content": str(output.get("content", "")),
                        "raw": output,
                    },
                    "metrics": output.get("session_state", {}),
                    "artifacts": [],
                    "error": None,
                }
                validate_adapter_doc("adapter-output", normalized)
                return normalized
            raise ValueError(
                "Adapter HTTP response must be lab.adapter.v1 for protocol "
                f"'{protocol or 'generic-json'}'"
            )
        except Exception as error:
            last_error = error
    raise RuntimeError(f"Adapter HTTP execution failed: {last_error}") from last_error


def _validate_adapter_doc(kind: str, data: dict[str, Any]) -> None:
    validate_schema_doc(f"{kind}.schema.json", data)


def _resolve_adapter_strict_contract(adapter: dict[str, Any]) -> bool:
    if "strict_contract" in adapter:
        configured = adapter.get("strict_contract")
        if isinstance(configured, bool):
            return configured
        if isinstance(configured, str):
            return configured.strip().lower() in {"1", "true", "yes", "on"}
        return bool(configured)
    return is_truthy(getenv("AGENT_SANDBOX_ADAPTER_STRICT_CONTRACT"))


def _resolve_isolation_mode(run_spec: dict[str, Any]) -> str:
    execution = run_spec.get("execution", {})
    isolation = str(execution.get("isolation", "sandbox_only")).strip().lower()
    if isolation not in {"sandbox_only", "allow_live"}:
        raise ValueError(f"Unsupported run execution.isolation: {isolation}")
    return isolation


def _sandbox_http_allowed_hosts() -> set[str]:
    raw = getenv("AGENT_SANDBOX_SANDBOX_HTTP_HOSTS", default="") or ""
    if not raw.strip():
        return {"localhost", "127.0.0.1", "::1", "host.docker.internal"}
    return {item.strip().lower() for item in raw.split(",") if item.strip()}


def _enforce_http_isolation(*, isolation_mode: str, protocol: str, url: str) -> None:
    if isolation_mode != "sandbox_only":
        return
    if protocol == "agno-agentos-workflow":
        raise ValueError(
            "execution.adapter.config.protocol=agno-agentos-workflow is blocked in "
            "sandbox_only mode. Set execution.isolation=allow_live to opt in."
        )
    parsed = urllib.parse.urlparse(url)
    host = (parsed.hostname or "").strip().lower()
    allowed_hosts = _sandbox_http_allowed_hosts()
    if not host or host not in allowed_hosts:
        allowed = ", ".join(sorted(allowed_hosts))
        raise ValueError(
            "execution.adapter.config.url host is not allowed in sandbox_only mode: "
            f"{host or '(empty)'}. Allowed: {allowed}"
        )
