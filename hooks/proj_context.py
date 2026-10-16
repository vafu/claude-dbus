#!/usr/bin/env python3
"""Inject proj goal context into agent sessions.

SessionStart: emits the active proj goal, in-progress subtasks, note path,
and a directive to keep proj current. UserPromptSubmit: emits a one-line
reminder with the active goal. Never fails: any error exits 0 with no output.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJ = str(Path.home() / ".local/bin/proj")
CACHE_TTL_SECONDS = 15
PROJ_TIMEOUT_SECONDS = 5

DIRECTIVE = (
    "proj tracking is MANDATORY for this session: read the active goal's note "
    "before planning, keep statuses current (`proj goal start/complete/status`), "
    "and append progress, decisions, blockers, and verification evidence to the "
    "note before ending work. Create goals for substantial new requests instead "
    "of leaving work untracked."
)


def run_proj(args: list[str], cwd: str) -> subprocess.CompletedProcess | None:
    try:
        return subprocess.run(
            [PROJ, *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=PROJ_TIMEOUT_SECONDS,
        )
    except Exception:
        return None


def cache_path(cwd: str) -> Path:
    key = hashlib.sha1(cwd.encode()).hexdigest()[:16]
    return Path(tempfile.gettempdir()) / f"proj-context-{key}.json"


def load_state(cwd: str) -> dict | None:
    path = cache_path(cwd)
    try:
        cached = json.loads(path.read_text())
        if time.time() - cached.get("ts", 0) < CACHE_TTL_SECONDS:
            return cached["state"]
    except Exception:
        pass

    active = run_proj(["goal", "active"], cwd)
    if active is None or active.returncode != 0:
        return None
    active_text = active.stdout.strip()

    listing = run_proj(["goal", "list", "--json"], cwd)
    if listing is None or listing.returncode != 0:
        return None
    try:
        goals = json.loads(listing.stdout)
    except ValueError:
        return None

    state = {"registered": True, "active": None, "in_progress": []}
    if active_text and active_text != "null":
        try:
            state["active"] = json.loads(active_text)
        except ValueError:
            pass
    state["in_progress"] = [g for g in goals if g.get("status") == "in-progress"]
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"ts": time.time(), "state": state}))
    except Exception:
        pass
    return state


def short(goal_id: str) -> str:
    return goal_id[:8]


def session_start_context(cwd: str) -> str:
    state = load_state(cwd)
    if state is None:
        return (
            f"proj: `{cwd}` is not a registered project. If this is intentional, "
            "long-lived work, register it with `proj add /absolute/checkout "
            '--name "Project name"`. '
        ) + DIRECTIVE

    lines = []
    active = state["active"]
    if active:
        lines.append(
            f"proj active goal: \"{active['title']}\" ({short(active['id'])}) "
            f"[{active['status']}]"
        )
        if active.get("note_path"):
            lines.append(f"proj active note: {active['note_path']}")
    else:
        lines.append("proj: no active goal selected.")

    in_progress = state["in_progress"]
    if in_progress:
        lines.append("proj in-progress goals:")
        for goal in in_progress[:8]:
            lines.append(f"  - \"{goal['title']}\" ({short(goal['id'])})")
    lines.append(DIRECTIVE)
    return "\n".join(lines)


def prompt_context(cwd: str) -> str:
    state = load_state(cwd)
    if state is None:
        return ""
    active = state["active"]
    if not active:
        return (
            "proj: no active goal for this project — register substantial work "
            "with `proj goal add` and `proj goal start`."
        )
    return (
        f"proj: active goal \"{active['title']}\" ({short(active['id'])}) — "
        "keep statuses and the goal note current as you work."
    )


def emit(event: str, context: str) -> None:
    if not context:
        return
    print(
        json.dumps(
            {
                "hookSpecificOutput": {
                    "hookEventName": event,
                    "additionalContext": context,
                }
            }
        )
    )


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        payload = {}
    cwd = payload.get("cwd") or str(Path.cwd())
    event = payload.get("hook_event_name", "")

    if event == "SessionStart":
        emit(event, session_start_context(cwd))
    elif event == "UserPromptSubmit":
        emit(event, prompt_context(cwd))
    return 0


if __name__ == "__main__":
    sys.exit(main())
