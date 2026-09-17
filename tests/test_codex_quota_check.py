"""Offline native evidence must never become a session mutation capability."""

import json
import os
from pathlib import Path
import subprocess


def test_offline_checker_survives_restart_without_transport_or_state_changes(
    tmp_path: Path, workmux_exe_path: Path
) -> None:
    fixture = json.loads(
        (
            Path(__file__).parent.parent
            / "resources/codex/quota-recovery/schema-projection.json"
        ).read_text()
    )
    schemas = tmp_path / "v2"
    schemas.mkdir()
    for name, schema in fixture["schemas"].items():
        (schemas / f"{name}.json").write_text(json.dumps(schema))

    turn = tmp_path / "turn.json"
    turn.write_text(
        json.dumps(
            {
                "method": "turn/completed",
                "params": {
                    "threadId": "original-thread",
                    "turn": {
                        "id": "failed-turn",
                        "status": "failed",
                        "error": {"codexErrorInfo": "usageLimitExceeded"},
                    },
                },
            }
        )
    )
    state = tmp_path / "state"
    state.mkdir()
    consent = state / "consent.json"
    consent.write_text('{"opt_in":true,"paused":true}')
    partial = tmp_path / "partial-work.txt"
    partial.write_text("unfinished work\n")
    sent = tmp_path / "transport-called"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    for name in ["codex", "tmux"]:
        fake = bin_dir / name
        fake.write_text('#!/bin/sh\n: > "$TRANSPORT_SENTINEL"\nexit 97\n')
        fake.chmod(0o755)
    env = {
        **os.environ,
        "HOME": str(tmp_path),
        "XDG_STATE_HOME": str(state),
        "PATH": str(bin_dir),
        "TRANSPORT_SENTINEL": str(sent),
    }
    args = [
        str(workmux_exe_path),
        "codex-quota-check",
        "--schema-dir",
        str(tmp_path),
        "--turn-event",
        str(turn),
    ]
    reports = []
    for _ in range(2):
        result = subprocess.run(
            args, cwd=tmp_path, env=env, capture_output=True, text=True
        )
        assert result.returncode == 0, result.stderr
        report = json.loads(result.stdout)
        assert report["recovery_enabled"] is False
        assert len(report["blockers"]) == 4
        assert report["turn"] == {
            "thread_id": "original-thread",
            "turn_id": "failed-turn",
            "outcome": "quota_failed",
        }
        reports.append(report)
    assert reports[0] == reports[1]
    rejected = subprocess.run(
        [*args, "--enable"], cwd=tmp_path, env=env, capture_output=True, text=True
    )
    assert rejected.returncode != 0
    assert not sent.exists()
    assert consent.read_text() == '{"opt_in":true,"paused":true}'
    assert partial.read_text() == "unfinished work\n"

    (schemas / "TurnStartParams.json").unlink()
    invalid = subprocess.run(
        args, cwd=tmp_path, env=env, capture_output=True, text=True
    )
    assert invalid.returncode != 0
    assert not invalid.stdout.strip()
    assert not sent.exists()
