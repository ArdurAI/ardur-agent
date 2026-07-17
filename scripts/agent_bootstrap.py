#!/usr/bin/env python3
"""Read-only bootstrap report for Ardur Agent sessions.

The script is intentionally dependency-free. It should be safe to run at the
start of any local agent session before the worker decides which issue,
worktree, or test path it owns.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import date
from typing import Any


LINEAR_PROJECT_NAME = "Agent Bootstrap & Coordination"
TEST_READINESS_PROJECT_NAME = "Test Readiness & Integration"
PLAN_LINEARIZATION_PROJECT_NAME = "Plan Corpus Linearization"
LINEAR_ORG_URL_KEY = "ardur-agent"
LINEAR_TEAM_KEY = "ARD"
LINEAR_KEYCHAIN_SERVICE = "LINEAR_API_KEY_ALL"
LINEAR_ARDUR_KEYCHAIN_SERVICE = "LINEAR_ARDUR_AGENT_KEY"
LINEAR_API_ENDPOINT = "https://api.linear.app/graphql"
LINEAR_ENV_KEYS = ("LINEAR_ARDUR_AGENT_KEY", "LINEAR_API_KEY")
LINEAR_KEYCHAIN_SERVICES = (LINEAR_ARDUR_KEYCHAIN_SERVICE, LINEAR_KEYCHAIN_SERVICE)
LINEAR_QUERY_TIMEOUT_SECONDS = 30
COMMAND_TIMEOUT_SECONDS = 8

SECRET_POSTURE_KEYS = [
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
    "OLLAMA_API_KEY",
    "QDRANT_URL",
    "QDRANT_API_KEY",
    "SLACK_BOT_TOKEN",
    "SLACK_SIGNING_SECRET",
    "MATRIX_ACCESS_TOKEN",
    "DISCORD_BOT_TOKEN",
    "TELEGRAM_BOT_TOKEN",
]

LOCAL_TOOL_KEYS = {
    "ollama": "Ollama local model daemon/client",
    "codex": "Codex CLI provider",
    "claude": "Claude CLI provider",
    "docker": "Docker/Qdrant local services",
    "cargo": "Rust build/test toolchain",
}

BOOTSTRAP_QUERY = r"""
query ArdurAgentBootstrap {
  organization {
    name
    urlKey
  }
  teams(first: 50) {
    nodes {
      id
      key
      name
    }
  }
  projects(first: 100) {
    nodes {
      id
      name
      url
      progress
      scope
      currentProgress
      targetDate
      state
    }
  }
  issues(first: 250, filter: { team: { key: { eq: "ARD" } } }) {
    nodes {
      identifier
      title
      url
      estimate
      priority
      updatedAt
      branchName
      state { name type }
      assignee { name }
      project { name url }
      labels(first: 30) { nodes { name } }
    }
  }
}
"""


def run_command(
    args: list[str],
    cwd: pathlib.Path | None = None,
    stdin: str | None = None,
    timeout: int = COMMAND_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    try:
        completed = subprocess.run(
            args,
            cwd=str(cwd) if cwd else None,
            input=stdin,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as exc:
        return {"ok": False, "stdout": "", "stderr": str(exc), "returncode": 127}
    except subprocess.TimeoutExpired as exc:
        return {
            "ok": False,
            "stdout": exc.stdout or "",
            "stderr": f"command timed out after {timeout}s",
            "returncode": 124,
        }

    return {
        "ok": completed.returncode == 0,
        "stdout": completed.stdout.strip(),
        "stderr": completed.stderr.strip(),
        "returncode": completed.returncode,
    }


def git_root(start: pathlib.Path) -> pathlib.Path:
    result = run_command(["git", "rev-parse", "--show-toplevel"], cwd=start)
    if not result["ok"] or not result["stdout"]:
        return start.resolve()
    return pathlib.Path(result["stdout"]).resolve()


def resolve_primary_root(repo_root: pathlib.Path) -> pathlib.Path:
    parts = repo_root.resolve().parts
    if "dev-workspace" not in parts:
        return repo_root.resolve()
    idx = parts.index("dev-workspace")
    if idx == 0:
        return repo_root.resolve()
    return pathlib.Path(*parts[:idx])


def format_progress_percent(progress: float | int | str | None) -> str:
    try:
        value = float(progress or 0)
    except (TypeError, ValueError):
        value = 0.0
    if value > 1:
        value = value / 100
    value = min(1.0, max(0.0, value))
    return f"{round(value * 100):.0f}%"


def display_number(value: float) -> int | float:
    return int(value) if value == int(value) else round(value, 2)


def issue_estimate(issue: dict[str, Any]) -> float:
    estimate = issue.get("estimate")
    if estimate is None:
        return 1.0
    try:
        return max(0.0, float(estimate))
    except (TypeError, ValueError):
        return 1.0


def state_type(issue: dict[str, Any]) -> str:
    state = issue.get("state") or {}
    return str(state.get("type") or "unknown")


def is_active_issue(issue: dict[str, Any]) -> bool:
    return state_type(issue) not in {"completed", "canceled", "duplicate"}


def issue_labels(issue: dict[str, Any]) -> set[str]:
    labels = ((issue.get("labels") or {}).get("nodes")) or []
    return {str(label.get("name")) for label in labels if label.get("name")}


def has_label(issue: dict[str, Any], label: str) -> bool:
    return label in issue_labels(issue)


def project_name(issue: dict[str, Any]) -> str:
    return str((issue.get("project") or {}).get("name") or "")


def is_parallel_ready(issue: dict[str, Any]) -> bool:
    return has_label(issue, "parallel:ready")


def is_parallel_owned(issue: dict[str, Any]) -> bool:
    return has_label(issue, "parallel:owned") or state_type(issue) == "started"


def is_stitch_issue(issue: dict[str, Any]) -> bool:
    return has_label(issue, "integration:stitch")


def candidate_sort_key(issue: dict[str, Any]) -> tuple[int, int, str]:
    priority = issue.get("priority")
    try:
        priority_value = int(priority)
    except (TypeError, ValueError):
        priority_value = 4
    plan_penalty = 1 if project_name(issue) == PLAN_LINEARIZATION_PROJECT_NAME else 0
    return (priority_value or 4, plan_penalty, str(issue.get("identifier") or ""))


def parallel_ready_candidates(issues: list[dict[str, Any]]) -> list[dict[str, Any]]:
    candidates = [
        issue
        for issue in issues
        if is_active_issue(issue)
        and is_parallel_ready(issue)
        and not is_parallel_owned(issue)
        and not is_stitch_issue(issue)
    ]
    return sorted(candidates, key=candidate_sort_key)


def compute_issue_progress(issues: list[dict[str, Any]]) -> dict[str, Any]:
    scoped = [issue for issue in issues if state_type(issue) not in {"canceled", "duplicate"}]
    scope = sum(issue_estimate(issue) for issue in scoped)
    completed = sum(issue_estimate(issue) for issue in scoped if state_type(issue) == "completed")
    progress = completed / scope if scope else 0.0
    return {
        "completed_estimate": display_number(completed),
        "scope_estimate": display_number(scope),
        "progress": progress,
        "percent": format_progress_percent(progress),
        "issue_count": len(scoped),
        "completed_issue_count": sum(1 for issue in scoped if state_type(issue) == "completed"),
    }


def secret_posture(env: dict[str, str] | os._Environ[str] = os.environ) -> dict[str, Any]:
    return {
        "provider": env.get("ARDUR_PROVIDER") or "anthropic (default)",
        "memory": env.get("ARDUR_MEMORY") or "in_memory (default)",
        "sensitive_inputs": {
            key: "present" if env.get(key) else "missing" for key in SECRET_POSTURE_KEYS
        },
    }


def local_tool_posture() -> dict[str, str]:
    posture: dict[str, str] = {}
    for binary, description in LOCAL_TOOL_KEYS.items():
        result = run_command(["command", "-v", binary], timeout=2)
        if not result["ok"]:
            result = run_command(["which", binary], timeout=2)
        posture[binary] = "present" if result["ok"] and result["stdout"] else "missing"
    return posture


def git_state(repo_root: pathlib.Path, primary_root: pathlib.Path) -> dict[str, Any]:
    branch = run_command(["git", "branch", "--show-current"], cwd=repo_root)
    status = run_command(["git", "status", "--short", "--branch"], cwd=repo_root)
    hooks = run_command(["git", "config", "--get", "core.hooksPath"], cwd=repo_root)
    worktrees = run_command(["git", "worktree", "list", "--porcelain"], cwd=repo_root)
    git_dir = run_command(["git", "rev-parse", "--git-dir"], cwd=repo_root)
    git_common = run_command(["git", "rev-parse", "--git-common-dir"], cwd=repo_root)

    status_lines = status["stdout"].splitlines() if status["stdout"] else []
    dirty_lines = [line for line in status_lines if not line.startswith("##")]
    worktree_paths = [
        line.split(" ", 1)[1]
        for line in worktrees["stdout"].splitlines()
        if line.startswith("worktree ")
    ]
    isolated_by_path = repo_root.resolve() != primary_root.resolve()
    linked_worktree = bool(git_dir["stdout"] and git_common["stdout"] and git_dir["stdout"] != git_common["stdout"])

    return {
        "repo_root": str(repo_root),
        "primary_root": str(primary_root),
        "branch": branch["stdout"] or "<detached-or-unknown>",
        "status": status["stdout"],
        "dirty_file_count": len(dirty_lines),
        "hooks_path": hooks["stdout"] or "<unset>",
        "worktrees": worktree_paths,
        "isolated_workspace": isolated_by_path or linked_worktree,
    }


def read_existing_file(path: pathlib.Path, max_chars: int = 4096) -> str:
    try:
        return path.read_text(encoding="utf-8")[:max_chars]
    except OSError:
        return ""


def local_facts(primary_root: pathlib.Path) -> dict[str, Any]:
    run_md = read_existing_file(primary_root / "RUN.md", 12000)
    e2e_readme = read_existing_file(primary_root / "crates" / "e2e-tests" / "README.md", 6000)
    return {
        "identity": "Ardur Agent is a secure agent substrate intended as a stronger alternative to Hermes Agent and OpenClaw.",
        "substrate": [
            "cap-token authorization",
            "Cedar policy evaluation",
            "cost-gate admission",
            "provider runtime and selector",
            "ES256 receipt chain",
            "session journals",
            "bi-temporal memory with optional Qdrant/hybrid retrieval",
            "fused runtime across CLI/server/channel paths",
        ],
        "runbook_present": bool(run_md),
        "e2e_stub_no_key": "need no API key" in e2e_readme,
        "known_dev_fidelity_gap": "dev fidelity, not production" in run_md,
    }


def linear_helper(primary_root: pathlib.Path) -> pathlib.Path:
    return primary_root / "architect" / "tools" / "linear_graphql.py"


def linear_recovery_steps(primary_root: pathlib.Path) -> list[str]:
    return [
        f"Do not claim non-`{LINEAR_TEAM_KEY}` issues from the generic Linear connector.",
        f"For internal ARD work, provide `LINEAR_ARDUR_AGENT_KEY`/`LINEAR_API_KEY` or a Keychain item named `{LINEAR_ARDUR_KEYCHAIN_SERVICE}`.",
        f"If the private helper exists, verify ARD access directly: `python3 {linear_helper(primary_root)} - < /tmp/ardur-linear-check.graphql`.",
        "For public contributors without ARD Linear credentials, use GitHub Issues/PRs and keep Linear claiming optional.",
        f"Expected Linear workspace URL key: `{LINEAR_ORG_URL_KEY}`; expected team key: `{LINEAR_TEAM_KEY}`.",
    ]


def validate_linear_workspace(payload: dict[str, Any]) -> tuple[bool, str | None, dict[str, Any]]:
    data = payload.get("data") or {}
    organization = data.get("organization") or {}
    teams = ((data.get("teams") or {}).get("nodes")) or []
    org_url_key = organization.get("urlKey")
    team_keys = {team.get("key") for team in teams}

    metadata = {
        "organization": organization,
        "teams": teams,
        "expected_org_url_key": LINEAR_ORG_URL_KEY,
        "expected_team_key": LINEAR_TEAM_KEY,
    }

    if org_url_key != LINEAR_ORG_URL_KEY:
        return (
            False,
            f"Linear workspace mismatch: expected `{LINEAR_ORG_URL_KEY}`, got `{org_url_key or '<unknown>'}`",
            metadata,
        )
    if LINEAR_TEAM_KEY not in team_keys:
        visible = ", ".join(sorted(str(key) for key in team_keys if key)) or "<none>"
        return (
            False,
            f"Linear team `{LINEAR_TEAM_KEY}` is not visible in workspace `{org_url_key}`; visible teams: {visible}",
            metadata,
        )
    return True, None, metadata


def linear_api_key_candidates(env: dict[str, str] | None = None) -> list[tuple[str, str]]:
    """Return ARD Linear API-key candidates without logging secret values."""
    source_env = os.environ if env is None else env
    candidates: list[tuple[str, str]] = []
    seen: set[str] = set()

    for name in LINEAR_ENV_KEYS:
        value = source_env.get(name)
        if value and value not in seen:
            candidates.append((f"env:{name}", value))
            seen.add(value)

    if sys.platform == "darwin":
        for service in LINEAR_KEYCHAIN_SERVICES:
            commands = [
                ["security", "find-generic-password", "-s", service, "-w"],
                [
                    "security",
                    "find-generic-password",
                    "-a",
                    source_env.get("USER", ""),
                    "-s",
                    service,
                    "-w",
                ],
            ]
            for command in commands:
                result = run_command(command, timeout=2)
                value = result["stdout"].strip() if result["ok"] else ""
                if value and value not in seen:
                    candidates.append((f"keychain:{service}", value))
                    seen.add(value)
                    break

    return candidates


def query_linear_direct_payload() -> dict[str, Any]:
    """Query Linear directly when the private helper is absent."""
    candidates = linear_api_key_candidates()
    if not candidates:
        return {
            "ok": False,
            "error": "Linear helper missing and no ARD Linear API key found in env or Keychain",
        }

    failures: list[str] = []
    body = json.dumps({"query": BOOTSTRAP_QUERY}).encode("utf-8")
    for source, api_key in candidates:
        request = urllib.request.Request(
            LINEAR_API_ENDPOINT,
            data=body,
            headers={"Authorization": api_key, "Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=LINEAR_QUERY_TIMEOUT_SECONDS) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except (OSError, urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
            failures.append(f"{source}: {type(exc).__name__}: {exc}")
            continue

        if payload.get("errors"):
            failures.append(f"{source}: {json.dumps(payload['errors'])}")
            continue

        workspace_ok, workspace_error, _metadata = validate_linear_workspace(payload)
        if workspace_ok:
            return {"ok": True, "payload": payload, "source": source}
        failures.append(f"{source}: {workspace_error}")

    return {
        "ok": False,
        "error": "; ".join(failures) or "No ARD Linear API key candidate reached the expected workspace",
    }


def query_linear(primary_root: pathlib.Path) -> dict[str, Any]:
    helper = linear_helper(primary_root)
    if helper.is_file():
        result = run_command(
            [sys.executable, str(helper), "-"],
            cwd=primary_root,
            stdin=BOOTSTRAP_QUERY,
            timeout=LINEAR_QUERY_TIMEOUT_SECONDS,
        )
        if not result["ok"]:
            return {
                "available": False,
                "error": result["stderr"] or result["stdout"] or "Linear query failed",
                "workspace": None,
                "teams": [],
                "recovery_steps": linear_recovery_steps(primary_root),
                "project": None,
                "active_issues": [],
                "parallel_ready_candidates": [],
            }

        try:
            payload = json.loads(result["stdout"])
        except json.JSONDecodeError as exc:
            return {
                "available": False,
                "error": f"Linear returned invalid JSON: {exc}",
                "workspace": None,
                "teams": [],
                "recovery_steps": linear_recovery_steps(primary_root),
                "project": None,
                "active_issues": [],
                "parallel_ready_candidates": [],
            }
    else:
        direct = query_linear_direct_payload()
        if not direct["ok"]:
            return {
                "available": False,
                "error": f"Linear helper missing at {helper}; {direct['error']}",
                "workspace": None,
                "teams": [],
                "recovery_steps": linear_recovery_steps(primary_root),
                "project": None,
                "active_issues": [],
                "parallel_ready_candidates": [],
            }
        payload = direct["payload"]

    if payload.get("errors"):
        return {
            "available": False,
            "error": json.dumps(payload["errors"]),
            "workspace": None,
            "teams": [],
            "recovery_steps": linear_recovery_steps(primary_root),
            "project": None,
            "active_issues": [],
            "parallel_ready_candidates": [],
        }

    workspace_ok, workspace_error, workspace_metadata = validate_linear_workspace(payload)
    if not workspace_ok:
        return {
            "available": False,
            "error": workspace_error,
            "workspace": workspace_metadata["organization"],
            "teams": workspace_metadata["teams"],
            "recovery_steps": linear_recovery_steps(primary_root),
            "project": None,
            "active_issues": [],
            "parallel_ready_candidates": [],
        }

    projects = payload.get("data", {}).get("projects", {}).get("nodes", [])
    project = next((p for p in projects if p.get("name") == LINEAR_PROJECT_NAME), None)
    all_issues = payload.get("data", {}).get("issues", {}).get("nodes", [])
    active_issues = [issue for issue in all_issues if is_active_issue(issue)]
    in_progress_issues = [issue for issue in active_issues if state_type(issue) == "started"]
    project_issues = [
        issue for issue in all_issues if (issue.get("project") or {}).get("name") == LINEAR_PROJECT_NAME
    ]
    completed_projects = [project for project in projects if float(project.get("progress") or 0) >= 1.0]
    project_by_name = {project.get("name"): project for project in projects}

    return {
        "available": True,
        "error": None,
        "workspace": workspace_metadata["organization"],
        "teams": workspace_metadata["teams"],
        "recovery_steps": [],
        "project": project,
        "test_readiness_project": project_by_name.get(TEST_READINESS_PROJECT_NAME),
        "plan_linearization_project": project_by_name.get(PLAN_LINEARIZATION_PROJECT_NAME),
        "completed_projects": completed_projects,
        "project_progress": summarize_project_progress(project) if project else None,
        "test_readiness_progress": summarize_project_progress(
            project_by_name.get(TEST_READINESS_PROJECT_NAME)
        ),
        "plan_linearization_progress": summarize_project_progress(
            project_by_name.get(PLAN_LINEARIZATION_PROJECT_NAME)
        ),
        "project_issue_progress": compute_issue_progress(project_issues),
        "in_progress_issues": in_progress_issues,
        "active_issues": active_issues,
        "parallel_ready_candidates": parallel_ready_candidates(active_issues),
    }


def bootstrap_installation(repo_root: pathlib.Path, primary_root: pathlib.Path) -> dict[str, Any]:
    primary_script = primary_root / "scripts" / "agent_bootstrap.py"
    repo_script = repo_root / "scripts" / "agent_bootstrap.py"
    return {
        "repo_script": str(repo_script),
        "primary_script": str(primary_script),
        "repo_script_present": repo_script.is_file(),
        "primary_script_present": primary_script.is_file(),
        "running_from_primary": repo_root.resolve() == primary_root.resolve(),
    }


def summarize_project_progress(project: dict[str, Any] | None) -> dict[str, Any] | None:
    if not project:
        return None
    current = project.get("currentProgress") or {}
    return {
        "name": project.get("name"),
        "url": project.get("url"),
        "targetDate": project.get("targetDate"),
        "progress": project.get("progress", 0),
        "percent": format_progress_percent(project.get("progress", 0)),
        "scope": project.get("scope", 0),
        "currentProgress": current,
    }


def build_context(start: pathlib.Path) -> dict[str, Any]:
    repo_root = git_root(start)
    primary_root = resolve_primary_root(repo_root)
    linear = query_linear(primary_root)
    return {
        "generated_on": date.today().isoformat(),
        "conduct_version": "2026-06-15",
        "linear_team": LINEAR_TEAM_KEY,
        "linear_project_name": LINEAR_PROJECT_NAME,
        "git": git_state(repo_root, primary_root),
        "linear": linear,
        "bootstrap_installation": bootstrap_installation(repo_root, primary_root),
        "local_facts": local_facts(primary_root),
        "provider_memory": secret_posture(os.environ),
        "local_tools": local_tool_posture(),
        "recommended_session_journal": str(
            primary_root / "architect" / "sessions" / f"{date.today().isoformat()}-<issue-slug>" / "journal.md"
        ),
    }


def issue_line(issue: dict[str, Any]) -> str:
    estimate = issue.get("estimate") if issue.get("estimate") is not None else "?"
    state = (issue.get("state") or {}).get("name", "unknown")
    project = (issue.get("project") or {}).get("name")
    suffix = f" [{project}]" if project else ""
    labels = sorted(label for label in issue_labels(issue) if label.startswith(("parallel:", "lane:", "integration:")))
    label_suffix = f" ({', '.join(labels)})" if labels else ""
    return f"- `{issue.get('identifier')}` {issue.get('title')} - {state}, estimate {estimate}{suffix}{label_suffix}: {issue.get('url')}"


def project_progress_line(label: str, progress: dict[str, Any] | None) -> str:
    if not progress:
        return f"- {label}: not found"
    return (
        f"- {label}: [{progress['name']}]({progress['url']}) "
        f"`{progress['percent']}` (scope `{progress['scope']}`, target `{progress.get('targetDate') or 'unset'}`)"
    )


def render_markdown(context: dict[str, Any]) -> str:
    git = context["git"]
    linear = context["linear"]
    facts = context["local_facts"]
    installation = context["bootstrap_installation"]
    posture = context["provider_memory"]
    tools = context["local_tools"]

    lines: list[str] = [
        "# Ardur Agent Session Bootstrap",
        "",
        f"Generated: {context['generated_on']}",
        "",
        "## Mandatory Conduct",
        "",
        "1. Run this bootstrap before planning or editing.",
        "2. Own exactly one Linear issue before implementation work.",
        "3. Use an isolated worktree for code edits; do not edit the primary dirty checkout.",
        "4. Keep a session journal under EXTENDED before moving the Linear issue to In Progress.",
        "5. Never print, commit, or paste secret values.",
        "6. After implementation, merge the branch to `dev` only after local checks and GitHub workflows pass; do not move to the next item before that merge is complete.",
        "",
        "## Source Hierarchy",
        "",
        "- Linear `ARD`: internal scope, state, priority, acceptance evidence, and progress.",
        "- EXTENDED drive: files, plans, journals, audits, and local evidence.",
        "- GitHub `ArdurAI/ardur-agent`: verified public code and PR surface.",
        "- Notion: searchable knowledge projection, not completion authority.",
        "",
        "## Project Identity",
        "",
        f"- {facts['identity']}",
        "- Core substrate: " + ", ".join(facts["substrate"]) + ".",
        f"- E2E stub path requires no API key: {'yes' if facts['e2e_stub_no_key'] else 'unknown'}.",
        f"- Current runbook flags dev-fidelity caveats: {'yes' if facts['known_dev_fidelity_gap'] else 'unknown'}.",
        "",
        "## Local Git State",
        "",
        f"- Repo root: `{git['repo_root']}`",
        f"- EXTENDED primary root: `{git['primary_root']}`",
        f"- Branch: `{git['branch']}`",
        f"- Isolated workspace: `{git['isolated_workspace']}`",
        f"- Dirty file count: `{git['dirty_file_count']}`",
        f"- Hooks path: `{git['hooks_path']}`",
        f"- Worktrees known: `{len(git['worktrees'])}`",
        "",
        "## Bootstrap Installation",
        "",
        f"- Script in this checkout: `{'present' if installation['repo_script_present'] else 'missing'}` at `{installation['repo_script']}`",
        f"- Script in EXTENDED primary checkout: `{'present' if installation['primary_script_present'] else 'missing'}` at `{installation['primary_script']}`",
    ]

    if not installation["primary_script_present"]:
        lines.extend(
            [
                "",
                "> WARNING: The bootstrap is not installed in the primary EXTENDED checkout. Agents started from primary `dev` may fall back to generic tools and see the wrong Linear workspace until this branch is merged.",
            ]
        )

    if not git["isolated_workspace"]:
        lines.extend(
            [
                "",
                "> WARNING: This appears to be the primary checkout. Create/use a `dev-workspace/<slug>` worktree before code edits.",
            ]
        )

    lines.extend(["", "## Linear Status", ""])
    if not linear["available"]:
        lines.append(f"- Linear unavailable: {linear['error']}")
        recovery_steps = linear.get("recovery_steps") or []
        if recovery_steps:
            lines.extend(["", "### Linear Recovery Steps", ""])
            lines.extend(f"- {step}" for step in recovery_steps)
    else:
        workspace = linear.get("workspace") or {}
        teams = linear.get("teams") or []
        team_names = ", ".join(
            f"{team.get('key')} ({team.get('name')})" for team in teams if team.get("key")
        )
        lines.extend(
            [
                f"- Workspace: `{workspace.get('urlKey')}` ({workspace.get('name')})",
                f"- Visible teams: `{team_names or '<none>'}`",
                f"- Access guard: expected workspace `{LINEAR_ORG_URL_KEY}`, team `{LINEAR_TEAM_KEY}`.",
            ]
        )
        project_progress = linear.get("project_progress")
        if project_progress:
            lines.extend(
                [
                    f"- Project: [{project_progress['name']}]({project_progress['url']})",
                    f"- Native project progress: `{project_progress['percent']}` (scope `{project_progress['scope']}`)",
                    f"- Target date: `{project_progress.get('targetDate') or 'unset'}`",
                    f"- Current progress raw counts: `{json.dumps(project_progress['currentProgress'], sort_keys=True)}`",
                ]
            )
            fallback = linear.get("project_issue_progress") or {}
            lines.append(
                f"- Computed issue progress fallback: `{fallback.get('percent', '0%')}` "
                f"({fallback.get('completed_estimate', 0)}/{fallback.get('scope_estimate', 0)} estimate)"
            )
        else:
            lines.append(f"- Project `{LINEAR_PROJECT_NAME}` not found.")

        lines.extend(
            [
                project_progress_line("Test readiness", linear.get("test_readiness_progress")),
                project_progress_line("Plan corpus linearization", linear.get("plan_linearization_progress")),
            ]
        )

        completed_projects = linear.get("completed_projects", [])[:8]
        lines.extend(["", "### Completed Linear Project Snapshot", ""])
        if completed_projects:
            lines.extend(
                f"- [{project.get('name')}]({project.get('url')}) - {format_progress_percent(project.get('progress'))}"
                for project in completed_projects
            )
        else:
            lines.append("- No completed projects returned in the current Linear window.")

        in_progress = linear.get("in_progress_issues", [])[:8]
        lines.extend(["", "### Current Work In Progress", ""])
        if in_progress:
            lines.extend(issue_line(issue) for issue in in_progress)
        else:
            lines.append("- No started ARD issues returned by Linear.")

        candidates = linear.get("parallel_ready_candidates", [])[:12]
        lines.extend(["", "### Parallel-Ready Candidates", ""])
        if candidates:
            lines.extend(issue_line(issue) for issue in candidates)
        else:
            lines.append("- No unowned `parallel:ready` ARD issues returned by Linear.")

        plan_candidates = [
            issue
            for issue in linear.get("parallel_ready_candidates", [])
            if project_name(issue) == PLAN_LINEARIZATION_PROJECT_NAME
        ][:12]
        lines.extend(["", "### Plan Verification Backlog", ""])
        if plan_candidates:
            lines.extend(issue_line(issue) for issue in plan_candidates)
        else:
            lines.append("- No plan-verification candidates returned by Linear.")

        pending = [
            issue
            for issue in linear.get("active_issues", [])
            if state_type(issue) != "started" and not is_parallel_ready(issue)
        ][:12]
        lines.extend(["", "### Pending / Open ARD Issues", ""])
        if pending:
            lines.extend(issue_line(issue) for issue in pending)
        else:
            lines.append("- No pending ARD issues returned by Linear.")

    lines.extend(
        [
            "",
            "## Provider, Memory, and Tool Posture",
            "",
            f"- Provider selector: `{posture['provider']}`",
            f"- Memory selector: `{posture['memory']}`",
            "- Sensitive inputs: "
            + ", ".join(f"`{key}={value}`" for key, value in posture["sensitive_inputs"].items()),
            "- Local tools: " + ", ".join(f"`{tool}={state}`" for tool, state in tools.items()),
            "",
            "## Parallel Session Rules",
            "",
            "- Before edits, claim/update one Linear issue with branch, worktree, session journal, and expected files.",
            "- Do not edit files owned by another active issue unless a Linear handoff comment names the reason and scope.",
            "- Prefer unowned `parallel:ready` issues; never claim `parallel:owned` without an explicit handoff.",
            "- Treat `integration:stitch` issues as coordination targets until related lane evidence is available.",
            "- For `Verify/implement plan` issues, first prove whether the source plan is already implemented before coding.",
            "- Prefer `git worktree add dev-workspace/<slug> -b gnanirahulnutakki/<issue>-<slug> origin/dev` from the EXTENDED primary root.",
            f"- Suggested journal path: `{context['recommended_session_journal']}`",
            "",
            "## Implementation Promotion Gate",
            "",
            "A session is not allowed to move to the next Linear item until all of these are true:",
            "",
            "1. The issue branch has fresh local verification evidence in Linear.",
            "2. The branch is pushed and reviewed or merged through the agreed GitHub path.",
            "3. Every required GitHub workflow for the branch or PR is green.",
            "4. The implementation branch is merged into `dev`.",
            "5. Local `dev` is updated from origin and the final smoke check still passes.",
            "6. Linear is updated with the merge commit/PR, workflow evidence, and final status.",
            "",
            "## Test-By-Tomorrow Smoke Path",
            "",
            "Reference docs:",
            "",
            "- [RUN.md](RUN.md) for provider, memory, server, and known-gap details.",
            "- [crates/e2e-tests/README.md](crates/e2e-tests/README.md) for no-key fused-substrate scenarios.",
            "- [crates/cli/README.md](crates/cli/README.md) for CLI offline/stub behavior.",
            "",
            "Required no-key baseline:",
            "",
            "```sh",
            "cargo test -p ardur-e2e-tests",
            "cargo test -p ardur-server --test boot_smoke",
            "cargo test -p ardur-cli --test cli_smoke_echo",
            "cargo build --workspace --bins",
            "```",
            "",
            "Optional live-provider checks only after confirming credentials or local CLIs are present; never print key values.",
            "",
            "## Next Action",
            "",
            "Pick one unowned `parallel:ready` Linear issue, create/use its isolated worktree, add the session journal path as a Linear comment, verify existing implementation before coding, and do not start another item until this branch is merged to `dev` with workflows green.",
        ]
    )
    return "\n".join(lines) + "\n"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Print the Ardur Agent session bootstrap report.")
    parser.add_argument("--json", action="store_true", help="emit machine-readable JSON instead of Markdown")
    parser.add_argument(
        "--start",
        type=pathlib.Path,
        default=pathlib.Path.cwd(),
        help="path inside the repo to bootstrap from; defaults to current directory",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    context = build_context(args.start.resolve())
    if args.json:
        print(json.dumps(context, indent=2, sort_keys=True))
    else:
        print(render_markdown(context), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
