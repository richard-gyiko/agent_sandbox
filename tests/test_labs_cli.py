from __future__ import annotations

import argparse
import json

from agent_sandbox import cli


def test_slug_id() -> None:
    assert cli._slug_id("My Scenario 01") == "my-scenario-01"
    assert cli._slug_id("___") == "unnamed"


def test_init_scenario_and_run_templates(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("AGENT_SANDBOX_V3_DIR", str(tmp_path))

    scenario_args = argparse.Namespace(kind="scenario", name="Demo Scenario", force=False)
    assert cli._cmd_init(scenario_args) == 0
    scenario_file = tmp_path / "scenarios" / "demo-scenario.yaml"
    assert scenario_file.exists()

    run_args = argparse.Namespace(
        kind="run",
        name="Demo Run",
        scenario="demo-scenario",
        environment="local_twins",
        target="workflow.email_document_processor",
        force=False,
    )
    assert cli._cmd_init(run_args) == 0
    run_file = tmp_path / "runs" / "demo-run.yaml"
    assert run_file.exists()


def test_doctor_json_output(monkeypatch, capsys) -> None:
    class FakeObserv:
        available = True
        base_url = "http://localhost:7080"
        reason = ""

    def _ensure_twins_available(*_a, **_k):
        return None

    def _get_observability_status():
        return FakeObserv()

    def _list_capabilities():
        return {
            "assertions": ["a"],
            "actions": ["b"],
            "adapters": {"http_protocols": ["p"]},
        }

    monkeypatch.setattr("agent_sandbox.cli.ensure_twins_available", _ensure_twins_available)
    monkeypatch.setattr("agent_sandbox.cli.get_observability_status", _get_observability_status)
    monkeypatch.setattr("agent_sandbox.cli.list_capabilities", _list_capabilities)
    monkeypatch.setattr("agent_sandbox.cli.list_runs", lambda: [])

    args = argparse.Namespace(timeout_s=0.1, check_runs=False, json=True)
    code = cli._cmd_doctor(args)
    assert code == 0
    out = capsys.readouterr().out
    payload = json.loads(out)
    assert payload["ok"] is True


def test_write_junit_report(tmp_path) -> None:
    out = tmp_path / "report.xml"
    summary = {
        "tier": "p0-smoke",
        "runs": [
            {
                "run_id": "ok-run",
                "scenario_id": "s1",
                "status": "COMPLETED",
                "assertions_passed": True,
            },
            {
                "run_id": "bad-run",
                "scenario_id": "s2",
                "status": "FAILED",
                "assertions_passed": False,
            },
        ],
    }
    cli._write_junit_report(str(out), summary)
    text = out.read_text(encoding="utf-8")
    assert "testsuite" in text
    assert 'failures="1"' in text
    assert "bad-run" in text


def test_run_passed_completed_with_assertions() -> None:
    report = {"status": "COMPLETED", "assertions_passed": True}
    assert cli._run_passed(report) is True


def test_write_report_recomputes_failed_runs(tmp_path) -> None:
    out = tmp_path / "summary.json"
    summary = {
        "tier": None,
        "total_runs": 999,
        "failed_runs": 999,
        "runs": [
            {"status": "COMPLETED", "assertions_passed": True},
            {"status": "FAILED", "assertions_passed": False},
        ],
    }
    cli._write_report(str(out), summary)
    payload = json.loads(out.read_text(encoding="utf-8"))
    assert payload["total_runs"] == 2
    assert payload["failed_runs"] == 1


def test_doctor_text_output_includes_fix_hint(monkeypatch, capsys) -> None:
    class _Obs:
        available = True
        base_url = "http://x"
        reason = ""

    def _get_observability_status():
        return _Obs()

    monkeypatch.setattr(
        "agent_sandbox.cli.ensure_twins_available",
        lambda *_a, **_k: (_ for _ in ()).throw(RuntimeError("down")),
    )
    monkeypatch.setattr("agent_sandbox.cli.get_observability_status", _get_observability_status)
    monkeypatch.setattr(
        "agent_sandbox.cli.list_capabilities",
        lambda: {"assertions": [], "actions": [], "adapters": {"http_protocols": []}},
    )
    args = argparse.Namespace(timeout_s=0.1, check_runs=False, json=False)
    code = cli._cmd_doctor(args)
    assert code == 1
    out = capsys.readouterr().out
    assert "fix:" in out
