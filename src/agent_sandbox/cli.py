"""CLI for labs environment and scenario operations."""

from __future__ import annotations

import argparse
import importlib.resources as pkg_resources
import json
import sys
import xml.etree.ElementTree as ET
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from agent_sandbox.assertions import assert_scenario_expectations
from agent_sandbox.dsl import (
    list_run_ids_for_tier,
    list_runs,
    list_scenarios,
    load_environment,
    load_run,
    load_scenario,
    resolve_environment_path,
    resolve_run_path,
    resolve_scenario_path,
    run_root,
    scenario_root,
)
from agent_sandbox.env import getenv, scoped_env
from agent_sandbox.registry import list_capabilities, list_registered_targets
from agent_sandbox.runner import execute_run_spec
from agent_sandbox.runtime import (
    default_endpoints,
    ensure_twins_available,
    get_observability_status,
    reset_twins,
    run_agent_for_scenario,
    run_env_down,
    run_env_up,
    run_workflow_for_scenario,
    seed_twins,
    snapshot_twins,
)
from agent_sandbox.telemetry import new_run_id, observed_span, set_run_context
from agent_sandbox.validation import plugin_policy_env, validate_run_spec


def _cmd_env(args: argparse.Namespace) -> int:
    if args.action == "up":
        return run_env_up(args.compose_file)
    if args.action == "down":
        return run_env_down(args.compose_file, purge=not args.keep_volumes)
    return 1


def _cmd_scenario(args: argparse.Namespace) -> int:
    endpoints = default_endpoints()
    if args.action == "list":
        for path in list_scenarios():
            print(path.stem)
        return 0

    path = resolve_scenario_path(args.scenario)
    scenario = load_scenario(path)

    if args.action == "apply":
        ensure_twins_available(endpoints)
        if args.reset:
            reset_twins(endpoints)
        seed_twins(endpoints, scenario)
        print(f"Applied scenario: {scenario['meta']['name']}")
        return 0

    if args.action == "assert":
        ensure_twins_available(endpoints)
        snap = snapshot_twins(endpoints)
        assert_scenario_expectations(scenario, snap)
        print(f"Assertions passed: {scenario['meta']['name']}")
        return 0

    if args.action == "run":
        ensure_twins_available(endpoints)
        if args.reset:
            reset_twins(endpoints)
        seed_twins(endpoints, scenario)
        run_id = args.session_id or new_run_id()
        run_context = {
            "agent_sandbox.run.id": run_id,
            "agent_sandbox.scenario.name": scenario.get("meta", {}).get("name", ""),
            "agent_sandbox.scenario.version": scenario.get("version", 3),
            "agent_sandbox.client.mode": (getenv("WHIZY_RUNTIME_MODE", default="twin") or "twin"),
        }

        observ_overrides: dict[str, str] = {}
        observ_defaults: dict[str, str] = {}
        if not args.observability_off:
            observ = get_observability_status()
            if observ.available:
                observ_overrides["AGENT_SANDBOX_OBSERVABILITY_ENABLED"] = "true"
                observ_defaults["AGENT_SANDBOX_OTLP_ENDPOINT"] = f"{observ.base_url}/v1/traces"
                observ_defaults["AGENT_SANDBOX_OBSERV_SERVICE_NAME"] = "agent-sandbox"
                observ_defaults["OTEL_EXPORTER_OTLP_ENDPOINT"] = f"{observ.base_url}/v1/traces"
                observ_defaults["OTEL_SERVICE_NAME"] = "agent-sandbox"
            else:
                print(
                    "Warning: observability backend unavailable "
                    f"({observ.base_url}): {observ.reason}"
                )

        run_summary = {}
        configured_agent = scenario.get("run", {}).get("agent_id", "")
        agent_to_execute = args.execute_agent or configured_agent
        plugin_overrides = {"AGENT_SANDBOX_UNSAFE_PLUGINS": "true"} if args.unsafe_plugins else {}
        with (
            scoped_env(
                overrides={**observ_overrides, **plugin_overrides},
                defaults=observ_defaults,
            ),
            set_run_context(run_context),
            observed_span("agent_sandbox.run", attributes=run_context),
        ):
            if args.execute_workflow:
                run_summary = run_workflow_for_scenario(
                    scenario,
                    endpoints=endpoints,
                    session_id=run_id,
                )
                print(f"Workflow status: {run_summary.get('status')}")
            if agent_to_execute:
                run_summary = run_agent_for_scenario(
                    scenario,
                    endpoints=endpoints,
                    agent_id=agent_to_execute,
                )
                print(f"Agent result: {json.dumps(run_summary, ensure_ascii=False)}")
        snap = snapshot_twins(endpoints)
        if args.assert_after:
            assert_scenario_expectations(
                scenario,
                snap,
                workflow_metrics=run_summary.get("session_state"),
                run_metadata={"run_id": run_id},
            )
            print(f"Assertions passed: {scenario['meta']['name']}")
        return 0

    return 1


def _cmd_run(args: argparse.Namespace) -> int:
    endpoints = default_endpoints()
    if args.action == "list":
        if args.tier:
            for run_id in list_run_ids_for_tier(args.tier):
                print(run_id)
        else:
            for path in list_runs():
                print(path.stem)
        return 0

    if args.action == "targets":
        kind = args.kind if args.kind else None
        for item in list_registered_targets(kind=kind):
            print(item)
        return 0

    if args.action == "execute":
        report = _execute_named_run(
            args.run,
            endpoints=endpoints,
            reset=args.reset,
            assert_after=args.assert_after,
            session_id=args.session_id,
            observability_off=args.observability_off,
            print_progress=True,
            unsafe_plugins=args.unsafe_plugins,
        )
        print(f"Run result: {json.dumps(report['result'], ensure_ascii=False)}")
        if report["assertions_passed"]:
            print(f"Assertions passed: {report['scenario_name']}")
        summary = {
            "tier": None,
            "started_at": report["started_at"],
            "finished_at": report["finished_at"],
            "total_runs": 1,
            "failed_runs": 0 if _run_passed(report) else 1,
            "runs": [report],
        }
        report_out = args.report_out or _default_report_path(prefix=report["run_id"])
        _write_report(report_out, summary)
        if args.junit_out:
            _write_junit_report(args.junit_out, summary)
        return 0

    if args.action == "execute-tier":
        started = datetime.now(UTC).isoformat()
        run_ids = list_run_ids_for_tier(args.tier)
        if not run_ids:
            raise ValueError(f"No runs found for tier: {args.tier}")
        reports: list[dict[str, Any]] = []
        failed = 0
        for run_id in run_ids:
            try:
                report = _execute_named_run(
                    run_id,
                    endpoints=endpoints,
                    reset=not args.no_reset,
                    assert_after=not args.no_assert_after,
                    session_id="",
                    observability_off=args.observability_off,
                    print_progress=True,
                    unsafe_plugins=args.unsafe_plugins,
                )
                reports.append(report)
                if not _run_passed(report):
                    failed += 1
            except Exception as error:
                failed += 1
                reports.append(
                    {
                        "run_id": run_id,
                        "status": "FAILED",
                        "assertions_passed": False,
                        "error": str(error),
                    }
                )
                print(f"Run failed: {run_id}: {error}", file=sys.stderr)
                if args.stop_on_failure:
                    break
        ended = datetime.now(UTC).isoformat()
        summary = {
            "tier": args.tier,
            "started_at": started,
            "finished_at": ended,
            "total_runs": len(reports),
            "failed_runs": failed,
            "runs": reports,
        }
        report_out = args.report_out or _default_report_path(prefix=args.tier)
        _write_report(report_out, summary)
        if args.junit_out:
            _write_junit_report(args.junit_out, summary)
        print(
            json.dumps(
                {
                    "tier": args.tier,
                    "total_runs": len(reports),
                    "failed_runs": failed,
                }
            )
        )
        return 0 if failed == 0 else 1

    if args.action == "validate":
        run_path = resolve_run_path(args.run)
        run_spec = load_run(run_path)
        plugin_overrides = {"AGENT_SANDBOX_UNSAFE_PLUGINS": "true"} if args.unsafe_plugins else {}
        with scoped_env(overrides=plugin_overrides):
            report = validate_run_spec(run_spec)
        print(json.dumps(report, indent=2))
        return 0

    return 1


def _cmd_snapshot(args: argparse.Namespace) -> int:
    endpoints = default_endpoints()
    ensure_twins_available(endpoints)
    snap = snapshot_twins(endpoints)
    if args.out:
        path = Path(args.out)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(snap, indent=2), encoding="utf-8")
        print(f"Wrote snapshot: {path}")
    else:
        print(json.dumps(snap, indent=2))
    return 0


def _cmd_observ(args: argparse.Namespace) -> int:
    if args.action == "status":
        status = get_observability_status()
        state = "available" if status.available else "unavailable"
        print(f"DuckLens: {state}")
        print(f"URL: {status.base_url}")
        if status.reason:
            print(f"Reason: {status.reason}")
        return 0 if status.available else 1
    return 1


def _cmd_capabilities(args: argparse.Namespace) -> int:
    caps = list_capabilities()
    if args.json:
        print(json.dumps(caps, indent=2))
        return 0

    print("Adapters:")
    print(f"  types: {', '.join(caps['adapters']['types'])}")
    print(f"  http_protocols: {', '.join(caps['adapters']['http_protocols'])}")
    print("Assertions:")
    for kind in caps["assertions"]:
        print(f"  - {kind}")
    print("Actions:")
    for kind in caps["actions"]:
        print(f"  - {kind}")
    print("Targets:")
    print(f"  workflow: {', '.join(caps['targets']['workflow'])}")
    print(f"  agent: {', '.join(caps['targets']['agent'])}")
    print("Plugins:")
    print(f"  unsafe_enabled: {caps['plugins']['unsafe_enabled']}")
    print(f"  allowlist: {', '.join(caps['plugins']['allowlist']) or '(empty)'}")
    print(f"  loaded: {', '.join(caps['plugins']['loaded_modules']) or '(none)'}")
    return 0


def _slug_id(value: str) -> str:
    cleaned = "".join(ch if ch.isalnum() else "-" for ch in value.strip().lower())
    while "--" in cleaned:
        cleaned = cleaned.replace("--", "-")
    return cleaned.strip("-") or "unnamed"


def _write_template(path: Path, content: str, force: bool) -> None:
    if path.exists() and not force:
        raise FileExistsError(f"File already exists: {path} (use --force to overwrite)")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _load_template_text(filename: str) -> str:
    resource = (
        pkg_resources.files("agent_sandbox")
        .joinpath("resources")
        .joinpath("templates")
        .joinpath(filename)
    )
    if not resource.is_file():
        raise FileNotFoundError(f"Template not found: {filename}")
    return resource.read_text(encoding="utf-8")


def _cmd_init(args: argparse.Namespace) -> int:
    if args.kind == "scenario":
        scenario_id = _slug_id(args.name)
        path = scenario_root() / f"{scenario_id}.yaml"
        content = _load_template_text("scenario.yaml.tpl").format(
            scenario_id=scenario_id,
            name=args.name,
        )
        _write_template(path, content, force=args.force)
        print(f"Created scenario template: {path}")
        return 0

    if args.kind == "run":
        run_id = _slug_id(args.name)
        scenario_ref = _slug_id(args.scenario)
        path = run_root() / f"{run_id}.yaml"
        content = _load_template_text("run.yaml.tpl").format(
            run_id=run_id,
            name=args.name,
            environment=args.environment,
            scenario_ref=scenario_ref,
            target=args.target,
        )
        _write_template(path, content, force=args.force)
        print(f"Created run template: {path}")
        return 0

    return 1


def _cmd_doctor(args: argparse.Namespace) -> int:
    endpoints = default_endpoints()
    checks: list[tuple[str, bool, str]] = []

    checks.append(
        (
            "env.var.WHIZY_RUNTIME_MODE",
            True,
            getenv("WHIZY_RUNTIME_MODE", default="(unset)") or "(unset)",
        )
    )
    gmail_base = (
        getenv(
            "WHIZY_TWIN_GMAIL_BASE_URL",
            default=endpoints.gmail_base_url,
        )
        or endpoints.gmail_base_url
    )
    drive_base = (
        getenv(
            "WHIZY_TWIN_DRIVE_BASE_URL",
            default=endpoints.drive_base_url,
        )
        or endpoints.drive_base_url
    )
    checks.append(("env.var.WHIZY_TWIN_GMAIL_BASE_URL", True, gmail_base))
    checks.append(("env.var.WHIZY_TWIN_DRIVE_BASE_URL", True, drive_base))

    try:
        ensure_twins_available(endpoints, timeout_s=float(args.timeout_s))
        checks.append(("twins.reachable", True, "gmail+drive reachable"))
    except Exception as error:
        checks.append(("twins.reachable", False, str(error)))

    observ = get_observability_status()
    checks.append(
        (
            "observability.ducklens",
            observ.available,
            observ.base_url if observ.available else observ.reason or observ.base_url,
        )
    )

    try:
        caps = list_capabilities()
        checks.append(
            (
                "capabilities.registry",
                True,
                f"assertions={len(caps['assertions'])} actions={len(caps['actions'])} "
                f"protocols={len(caps['adapters']['http_protocols'])}",
            )
        )
    except Exception as error:
        checks.append(("capabilities.registry", False, str(error)))

    if args.check_runs:
        run_errors = 0
        for run_path in list_runs():
            try:
                run_spec = load_run(run_path)
                validate_run_spec(run_spec)
            except Exception:
                run_errors += 1
        checks.append(
            (
                "schema.run_specs",
                run_errors == 0,
                f"validated={len(list_runs())} failed={run_errors}",
            )
        )

    if args.json:
        print(
            json.dumps(
                {
                    "ok": all(item[1] for item in checks),
                    "checks": [
                        {
                            "name": name,
                            "ok": ok,
                            "details": details,
                            "remediation": _doctor_remediation(name, ok, details),
                        }
                        for name, ok, details in checks
                    ],
                },
                indent=2,
            )
        )
    else:
        for name, ok, details in checks:
            state = "OK" if ok else "FAIL"
            print(f"[{state}] {name}: {details}")
            if not ok:
                print(f"  fix: {_doctor_remediation(name, ok, details)}")

    return 0 if all(item[1] for item in checks) else 1


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="agent-sandbox")
    sub = parser.add_subparsers(dest="command", required=True)

    env = sub.add_parser("env", help="Manage twin environment")
    env.add_argument("action", choices=["up", "down"])
    env.add_argument("--compose-file", default="docker-compose.twins.yml")
    env.add_argument("--keep-volumes", action="store_true")
    env.set_defaults(handler=_cmd_env)

    scenario = sub.add_parser("scenario", help="Scenario operations")
    scenario_sub = scenario.add_subparsers(dest="action", required=True)

    s_list = scenario_sub.add_parser("list")
    s_list.set_defaults(handler=_cmd_scenario)

    s_apply = scenario_sub.add_parser("apply")
    s_apply.add_argument("scenario")
    s_apply.add_argument("--reset", action="store_true")
    s_apply.set_defaults(handler=_cmd_scenario)

    s_assert = scenario_sub.add_parser("assert")
    s_assert.add_argument("scenario")
    s_assert.set_defaults(handler=_cmd_scenario)

    s_run = scenario_sub.add_parser("run")
    s_run.add_argument("scenario")
    s_run.add_argument("--reset", action="store_true")
    s_run.add_argument("--assert-after", action="store_true")
    s_run.add_argument("--execute-workflow", action="store_true")
    s_run.add_argument("--execute-agent", default="")
    s_run.add_argument("--session-id", default="")
    s_run.add_argument("--observability-off", action="store_true")
    s_run.add_argument("--unsafe-plugins", action="store_true")
    s_run.set_defaults(handler=_cmd_scenario)

    run = sub.add_parser("run", help="Run-spec operations (v3)")
    run_sub = run.add_subparsers(dest="action", required=True)

    r_list = run_sub.add_parser("list")
    r_list.add_argument("--tier", choices=["p0-smoke", "p1-deep"], default="")
    r_list.set_defaults(handler=_cmd_run)

    r_targets = run_sub.add_parser("targets")
    r_targets.add_argument("--kind", choices=["workflow", "agent"], default="")
    r_targets.set_defaults(handler=_cmd_run)

    r_exec = run_sub.add_parser("execute")
    r_exec.add_argument("run")
    r_exec.add_argument("--reset", action="store_true")
    r_exec.add_argument("--assert-after", action="store_true")
    r_exec.add_argument("--session-id", default="")
    r_exec.add_argument("--observability-off", action="store_true")
    r_exec.add_argument("--unsafe-plugins", action="store_true")
    r_exec.add_argument("--report-out", default="")
    r_exec.add_argument("--junit-out", default="")
    r_exec.set_defaults(handler=_cmd_run)

    r_exec_tier = run_sub.add_parser("execute-tier")
    r_exec_tier.add_argument("tier", choices=["p0-smoke", "p1-deep"])
    r_exec_tier.add_argument("--no-reset", action="store_true")
    r_exec_tier.add_argument("--no-assert-after", action="store_true")
    r_exec_tier.add_argument("--stop-on-failure", action="store_true")
    r_exec_tier.add_argument("--observability-off", action="store_true")
    r_exec_tier.add_argument("--unsafe-plugins", action="store_true")
    r_exec_tier.add_argument("--report-out", default="")
    r_exec_tier.add_argument("--junit-out", default="")
    r_exec_tier.set_defaults(handler=_cmd_run)

    r_validate = run_sub.add_parser("validate")
    r_validate.add_argument("run")
    r_validate.add_argument("--unsafe-plugins", action="store_true")
    r_validate.set_defaults(handler=_cmd_run)

    snap = sub.add_parser("snapshot", help="Dump twin snapshot")
    snap.add_argument("--out", default="")
    snap.set_defaults(handler=_cmd_snapshot)

    observ = sub.add_parser("observ", help="Observability backend operations")
    observ.add_argument("action", choices=["status"])
    observ.set_defaults(handler=_cmd_observ)

    capabilities = sub.add_parser("capabilities", help="List labs runtime capabilities")
    capabilities.add_argument("--json", action="store_true")
    capabilities.set_defaults(handler=_cmd_capabilities)

    init = sub.add_parser("init", help="Generate labs spec templates")
    init_sub = init.add_subparsers(dest="kind", required=True)

    init_scenario = init_sub.add_parser("scenario")
    init_scenario.add_argument("name")
    init_scenario.add_argument("--force", action="store_true")
    init_scenario.set_defaults(handler=_cmd_init)

    init_run = init_sub.add_parser("run")
    init_run.add_argument("name")
    init_run.add_argument("--scenario", required=True)
    init_run.add_argument("--environment", default="local_twins")
    init_run.add_argument("--target", default="workflow.email_document_processor")
    init_run.add_argument("--force", action="store_true")
    init_run.set_defaults(handler=_cmd_init)

    doctor = sub.add_parser("doctor", help="Run labs health checks")
    doctor.add_argument("--timeout-s", default=5.0, type=float)
    doctor.add_argument("--check-runs", action="store_true")
    doctor.add_argument("--json", action="store_true")
    doctor.set_defaults(handler=_cmd_doctor)

    return parser


def main() -> int:
    parser = _build_parser()
    args = parser.parse_args()
    try:
        return args.handler(args)
    except Exception as error:  # pragma: no cover - CLI surface
        print(f"Error: {error}", file=sys.stderr)
        return 1


def _write_report(report_out: str, report: dict[str, Any]) -> None:
    runs = report.get("runs")
    if isinstance(runs, list):
        report["total_runs"] = len(runs)
        report["failed_runs"] = sum(0 if _run_passed(run) else 1 for run in runs)
    path = Path(report_out)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"Wrote report: {path}")


def _run_passed(report: dict[str, Any]) -> bool:
    status = str(report.get("status", "")).strip().upper()
    assertions_passed = report.get("assertions_passed")
    status_ok = status in {"COMPLETED", "SUCCESS", "OK"} or any(
        marker in status for marker in ("COMPLETED", "SUCCESS", "OK")
    )
    assertions_ok = assertions_passed is not False
    return status_ok and assertions_ok


def _write_junit_report(report_out: str, summary: dict[str, Any]) -> None:
    runs = summary.get("runs", [])
    tests = len(runs)
    failures = sum(0 if _run_passed(run) else 1 for run in runs)
    suite = ET.Element(
        "testsuite",
        name=str(summary.get("tier") or "agent-sandbox"),
        tests=str(tests),
        failures=str(failures),
    )
    for run in runs:
        case = ET.SubElement(
            suite,
            "testcase",
            classname=str(run.get("scenario_id") or "scenario"),
            name=str(run.get("run_id") or "run"),
            time="0",
        )
        if not _run_passed(run):
            status = str(run.get("status", "FAILED"))
            detail = run.get("error") or run.get("result", {})
            failure = ET.SubElement(case, "failure", message=f"status={status}")
            failure.text = json.dumps(detail, ensure_ascii=False)

    xml_data = ET.tostring(suite, encoding="unicode")
    path = Path(report_out)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(xml_data, encoding="utf-8")
    print(f"Wrote JUnit report: {path}")


def _doctor_remediation(name: str, ok: bool, _details: str) -> str:
    if ok:
        return ""
    if name == "twins.reachable":
        return "Start twins with `agent-sandbox env up` and confirm Docker is running."
    if name == "observability.ducklens":
        return (
            "Start observability backend or run commands with `--observability-off` "
            "for local smoke checks."
        )
    if name == "capabilities.registry":
        return (
            "Check plugin policy env (`AGENT_SANDBOX_PLUGIN_ALLOWLIST` or "
            "`AGENT_SANDBOX_UNSAFE_PLUGINS=true`) and re-run `agent-sandbox capabilities`."
        )
    if name == "schema.run_specs":
        return "Run `agent-sandbox run validate <run-id>` to identify and fix failing specs."
    if name.startswith("env.var."):
        return (
            "Export required AGENT_SANDBOX_* env values or use defaults from your environment spec."
        )
    return "Check runtime logs and re-run with --json for detailed diagnostics."


def _default_report_path(prefix: str) -> str:
    stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    return str(Path("artifacts") / "labs" / f"{prefix}-{stamp}.json")


def _execute_named_run(
    run_name: str,
    *,
    endpoints,
    reset: bool,
    assert_after: bool,
    session_id: str,
    observability_off: bool,
    print_progress: bool,
    unsafe_plugins: bool,
) -> dict[str, Any]:
    run_path = resolve_run_path(run_name)
    run_spec = load_run(run_path)
    plugin_overrides = {"AGENT_SANDBOX_UNSAFE_PLUGINS": "true"} if unsafe_plugins else {}
    with scoped_env(overrides=plugin_overrides):
        validate_run_spec(run_spec)
    env_spec = load_environment(resolve_environment_path(run_spec["environment_ref"]))
    policy_overrides = plugin_policy_env(env_spec)
    twins = env_spec.get("twins", {})
    endpoints.gmail_base_url = twins.get("gmail_base_url", endpoints.gmail_base_url)
    endpoints.drive_base_url = twins.get("drive_base_url", endpoints.drive_base_url)
    ensure_twins_available(endpoints)
    scenario = load_scenario(resolve_scenario_path(run_spec["scenario_ref"]))

    if reset:
        reset_twins(endpoints)
    seed_twins(endpoints, scenario)

    run_id = session_id or new_run_id()
    run_context = {
        "agent_sandbox.run.id": run_id,
        "agent_sandbox.run.spec_id": run_spec.get("meta", {}).get("id", ""),
        "agent_sandbox.scenario.name": scenario.get("meta", {}).get("name", ""),
        "agent_sandbox.scenario.version": scenario.get("version", 3),
        "agent_sandbox.client.mode": (getenv("WHIZY_RUNTIME_MODE", default="twin") or "twin"),
    }

    observ_overrides: dict[str, str] = {}
    observ_defaults: dict[str, str] = {}
    if not observability_off:
        observ = get_observability_status()
        if observ.available:
            observ_overrides["AGENT_SANDBOX_OBSERVABILITY_ENABLED"] = "true"
            observ_defaults["AGENT_SANDBOX_OTLP_ENDPOINT"] = f"{observ.base_url}/v1/traces"
            observ_defaults["AGENT_SANDBOX_OBSERV_SERVICE_NAME"] = "agent-sandbox"
            observ_defaults["OTEL_EXPORTER_OTLP_ENDPOINT"] = f"{observ.base_url}/v1/traces"
            observ_defaults["OTEL_SERVICE_NAME"] = "agent-sandbox"
        elif print_progress:
            print(
                f"Warning: observability backend unavailable ({observ.base_url}): {observ.reason}"
            )

    started = datetime.now(UTC).isoformat()
    with (
        scoped_env(
            overrides={**observ_overrides, **policy_overrides, **plugin_overrides},
            defaults=observ_defaults,
        ),
        set_run_context(run_context),
        observed_span("agent_sandbox.run", attributes=run_context),
    ):
        run_summary = execute_run_spec(run_spec, endpoints=endpoints, session_id=run_id)
    ended = datetime.now(UTC).isoformat()

    assertions_passed = False
    if assert_after:
        snap = snapshot_twins(endpoints)
        assert_scenario_expectations(
            scenario,
            snap,
            workflow_metrics=run_summary.get("session_state"),
            run_metadata={"run_id": run_id},
        )
        assertions_passed = True

    return {
        "run_id": run_spec.get("meta", {}).get("id", run_name),
        "run_name": run_spec.get("meta", {}).get("name", ""),
        "scenario_id": scenario.get("meta", {}).get("id", ""),
        "scenario_name": scenario.get("meta", {}).get("name", ""),
        "session_id": run_id,
        "started_at": started,
        "finished_at": ended,
        "status": run_summary.get("status", "unknown"),
        "assertions_passed": assertions_passed if assert_after else None,
        "result": run_summary,
    }


if __name__ == "__main__":
    raise SystemExit(main())
