"""
Tests for `workmux identity` and `workmux spawn`.

Together with `open --background --no-pane-cmds` these are the two-step launch a
supervisor needs: reserve the window and record what it is, then start the
process inside an identity that is already recorded. The tests check the ids
against what tmux itself reports, because an identity that merely agrees with
itself would be worthless to the caller recording it.
"""

import json
from pathlib import Path

import pytest

from .conftest import (
    MuxEnvironment,
    get_window_name,
    get_worktree_path,
    poll_until,
    run_workmux_add,
    run_workmux_command,
    write_workmux_config,
)

pytestmark = pytest.mark.tmux_only


def identity(
    env: MuxEnvironment, workmux_exe_path: Path, repo_path: Path, name: str
) -> dict:
    result = run_workmux_command(
        env, workmux_exe_path, repo_path, f"identity {name} --json"
    )
    return json.loads(result.stdout.strip())


def tmux_panes(env: MuxEnvironment, window_name: str) -> list[dict]:
    """What tmux says about one window, straight from the server."""
    result = env.mux_command(
        [
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_name}\t#{start_time}",
        ]
    )
    panes = []
    for line in result.stdout.strip().splitlines():
        pane_id, session, window_id, name, start_time = line.split("\t")
        if name == window_name:
            panes.append(
                {
                    "pane_id": pane_id,
                    "session": session,
                    "window_id": window_id,
                    "start_time": start_time,
                }
            )
    return panes


def test_identity_matches_what_tmux_reports(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Every id in the identity is the id tmux itself renders for that window."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-identity")
    window_name = get_window_name("feature-identity")

    reported = identity(env, workmux_exe_path, mux_repo_path, "feature-identity")
    truth = tmux_panes(env, window_name)

    assert truth, "the window under test has no panes"
    assert reported["is_open"] is True
    assert reported["handle"] == "feature-identity"
    assert reported["branch"] == "feature-identity"
    assert reported["path"] == str(get_worktree_path(mux_repo_path, "feature-identity"))
    assert reported["mode"] == "window"
    assert reported["backend"] == "tmux"
    assert reported["window_name"] == window_name
    assert reported["window_id"] == truth[0]["window_id"]
    assert reported["session"] == truth[0]["session"]
    assert reported["server_start_time"] == truth[0]["start_time"]
    assert sorted(reported["pane_ids"]) == sorted(pane["pane_id"] for pane in truth)


def test_a_worktree_without_a_window_is_reported_closed_rather_than_missing(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """A closed window and a missing worktree are different facts for a caller."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-closed")
    env.kill_window(get_window_name("feature-closed"))

    reported = identity(env, workmux_exe_path, mux_repo_path, "feature-closed")
    assert reported["is_open"] is False
    assert reported["window_id"] is None
    assert reported["session"] is None
    assert reported["window_name"] is None
    assert reported["pane_ids"] == []
    # The worktree facts are still there; only the live half is missing.
    assert reported["handle"] == "feature-closed"
    assert reported["path"] == str(get_worktree_path(mux_repo_path, "feature-closed"))

    missing = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "identity no-such-worktree --json",
        expect_fail=True,
    )
    assert missing.exit_code != 0


def test_open_can_prepare_a_window_without_running_the_agent(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """`--no-pane-cmds` gives a window whose identity exists before anything runs."""
    env = mux_server
    write_workmux_config(
        mux_repo_path, panes=[{"command": "echo AGENT-STARTED; sleep 60"}]
    )
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-bare")
    env.kill_window(get_window_name("feature-bare"))

    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "open feature-bare --background --no-pane-cmds",
    )
    reported = identity(env, workmux_exe_path, mux_repo_path, "feature-bare")
    assert reported["is_open"] is True
    assert reported["pane_ids"]

    content = env.capture_pane(get_window_name("feature-bare")) or ""
    assert "AGENT-STARTED" not in content


def test_spawn_replaces_the_recorded_pane_rather_than_splitting_a_new_one(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """The command becomes the pane, so the recorded pane id still describes it."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-spawn")
    env.kill_window(get_window_name("feature-spawn"))
    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        "open feature-spawn --background --no-pane-cmds",
    )
    reserved = identity(env, workmux_exe_path, mux_repo_path, "feature-spawn")
    pane_id = reserved["pane_ids"][0]

    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"spawn feature-spawn --pane {pane_id} --json -- "
        "sh -c 'echo SPAWNED-HERE; sleep 60'",
    )
    spawned = json.loads(result.stdout.strip())
    assert spawned["pane_id"] == pane_id
    assert spawned["window_id"] == reserved["window_id"]
    assert spawned["server_start_time"] == reserved["server_start_time"]

    # Captured from the pane id, not the window: the point of the test is that
    # the command runs in the pane the caller recorded, and capturing the
    # window would report whichever pane happens to be active.
    captured = ""

    def _ran() -> bool:
        nonlocal captured
        captured = env.mux_command(
            ["capture-pane", "-p", "-t", pane_id], check=False
        ).stdout
        return "SPAWNED-HERE" in captured

    assert poll_until(_ran, timeout=5.0), f"pane {pane_id} content: {captured!r}"

    # The pane the caller recorded is the one the command runs in, and it is
    # still the only one: a split would leave the recorded id on an idle shell
    # that outlives the command.
    after = identity(env, workmux_exe_path, mux_repo_path, "feature-spawn")
    assert pane_id in after["pane_ids"]
    assert after["window_id"] == reserved["window_id"]


def test_spawn_refuses_a_pane_from_another_window(
    mux_server: MuxEnvironment, workmux_exe_path: Path, mux_repo_path: Path
):
    """Respawning kills what is in the pane, so a wrong target is not a no-op."""
    env = mux_server
    write_workmux_config(mux_repo_path)
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-one")
    run_workmux_add(env, workmux_exe_path, mux_repo_path, "feature-two")

    stranger = identity(env, workmux_exe_path, mux_repo_path, "feature-two")
    result = run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"spawn feature-one --pane {stranger['pane_ids'][0]} -- true",
        expect_fail=True,
    )
    assert result.exit_code != 0
    assert "is not part of" in result.stderr
