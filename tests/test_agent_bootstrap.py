import importlib.util
import pathlib
import tempfile
import unittest


MODULE_PATH = pathlib.Path(__file__).resolve().parents[1] / "scripts" / "agent_bootstrap.py"


def load_bootstrap_module():
    spec = importlib.util.spec_from_file_location("agent_bootstrap", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class AgentBootstrapTests(unittest.TestCase):
    def setUp(self):
        self.bootstrap = load_bootstrap_module()

    def test_format_progress_percent_uses_linear_project_float(self):
        self.assertEqual(self.bootstrap.format_progress_percent(0), "0%")
        self.assertEqual(self.bootstrap.format_progress_percent(0.666666), "67%")
        self.assertEqual(self.bootstrap.format_progress_percent(1), "100%")

    def test_compute_issue_progress_uses_estimates_and_completed_state(self):
        issues = [
            {"estimate": 2, "state": {"type": "completed"}},
            {"estimate": 3, "state": {"type": "started"}},
            {"estimate": 1, "state": {"type": "unstarted"}},
        ]

        progress = self.bootstrap.compute_issue_progress(issues)

        self.assertEqual(progress["completed_estimate"], 2)
        self.assertEqual(progress["scope_estimate"], 6)
        self.assertEqual(progress["percent"], "33%")

    def test_primary_root_collapses_dev_workspace_worktree(self):
        root = pathlib.Path("/Volumes/EXTENDED/ardur-agent/dev-workspace/agent-bootstrap-linear")

        primary = self.bootstrap.resolve_primary_root(root)

        self.assertEqual(primary, pathlib.Path("/Volumes/EXTENDED/ardur-agent"))

    def test_secret_posture_never_returns_secret_values(self):
        env = {
            "ANTHROPIC_API_KEY": "sk-ant-sensitive",
            "ARDUR_PROVIDER": "anthropic",
            "ARDUR_MEMORY": "hybrid",
            "QDRANT_URL": "http://localhost:6334",
        }

        posture = self.bootstrap.secret_posture(env)

        rendered = repr(posture)
        self.assertIn("ANTHROPIC_API_KEY", rendered)
        self.assertIn("present", rendered)
        self.assertNotIn("sk-ant-sensitive", rendered)
        self.assertNotIn("localhost:6334", rendered)

    def test_parallel_ready_candidates_excludes_owned_started_and_stitch(self):
        issues = [
            {
                "identifier": "ARD-100",
                "priority": 3,
                "state": {"type": "unstarted"},
                "labels": {"nodes": [{"name": "parallel:ready"}]},
                "project": {"name": "Plan Corpus Linearization"},
            },
            {
                "identifier": "ARD-20",
                "priority": 2,
                "state": {"type": "unstarted"},
                "labels": {"nodes": [{"name": "parallel:ready"}]},
                "project": {"name": "Test Readiness & Integration"},
            },
            {
                "identifier": "ARD-18",
                "priority": 1,
                "state": {"type": "started"},
                "labels": {"nodes": [{"name": "parallel:ready"}]},
            },
            {
                "identifier": "ARD-41",
                "priority": 1,
                "state": {"type": "unstarted"},
                "labels": {"nodes": [{"name": "parallel:ready"}, {"name": "parallel:owned"}]},
            },
            {
                "identifier": "ARD-80",
                "priority": 1,
                "state": {"type": "unstarted"},
                "labels": {"nodes": [{"name": "parallel:ready"}, {"name": "integration:stitch"}]},
            },
        ]

        candidates = self.bootstrap.parallel_ready_candidates(issues)

        self.assertEqual([issue["identifier"] for issue in candidates], ["ARD-20", "ARD-100"])

    def test_render_markdown_includes_merge_before_next_item_gate(self):
        context = {
            "generated_on": "2026-06-15",
            "git": {
                "repo_root": "/Volumes/EXTENDED/ardur-agent/dev-workspace/test",
                "primary_root": "/Volumes/EXTENDED/ardur-agent",
                "branch": "gnanirahulnutakki/ARD-1-test",
                "isolated_workspace": True,
                "dirty_file_count": 0,
                "hooks_path": ".githooks",
                "worktrees": [],
            },
            "linear": {
                "available": False,
                "error": "offline",
                "recovery_steps": [],
            },
            "bootstrap_installation": {
                "repo_script": "/Volumes/EXTENDED/ardur-agent/dev-workspace/test/scripts/agent_bootstrap.py",
                "primary_script": "/Volumes/EXTENDED/ardur-agent/scripts/agent_bootstrap.py",
                "repo_script_present": True,
                "primary_script_present": True,
                "running_from_primary": False,
            },
            "local_facts": {
                "identity": "Ardur Agent test identity.",
                "substrate": ["cap-token authorization"],
                "e2e_stub_no_key": True,
                "known_dev_fidelity_gap": False,
            },
            "provider_memory": {
                "provider": "anthropic",
                "memory": "in_memory",
                "sensitive_inputs": {},
            },
            "local_tools": {},
            "recommended_session_journal": "/Volumes/EXTENDED/ardur-agent/architect/sessions/test/journal.md",
        }

        rendered = self.bootstrap.render_markdown(context)

        self.assertIn("Implementation Promotion Gate", rendered)
        self.assertIn("merged to `dev`", rendered)
        self.assertIn("workflows green", rendered)

    def test_validate_linear_workspace_rejects_wrong_organization(self):
        payload = {
            "data": {
                "organization": {"name": "Writing", "urlKey": "writing-technical"},
                "teams": {"nodes": [{"key": "WRI", "name": "Writing"}]},
            }
        }

        ok, error, metadata = self.bootstrap.validate_linear_workspace(payload)

        self.assertFalse(ok)
        self.assertIn("workspace mismatch", error)
        self.assertEqual(metadata["expected_org_url_key"], "ardur-agent")

    def test_validate_linear_workspace_rejects_missing_ard_team(self):
        payload = {
            "data": {
                "organization": {"name": "Ardur Agent", "urlKey": "ardur-agent"},
                "teams": {"nodes": [{"key": "WRI", "name": "Writing"}]},
            }
        }

        ok, error, _metadata = self.bootstrap.validate_linear_workspace(payload)

        self.assertFalse(ok)
        self.assertIn("ARD", error)

    def test_validate_linear_workspace_accepts_ard_team(self):
        payload = {
            "data": {
                "organization": {"name": "Ardur Agent", "urlKey": "ardur-agent"},
                "teams": {"nodes": [{"key": "ARD", "name": "Ardur Agent"}]},
            }
        }

        ok, error, metadata = self.bootstrap.validate_linear_workspace(payload)

        self.assertTrue(ok)
        self.assertIsNone(error)
        self.assertEqual(metadata["organization"]["urlKey"], "ardur-agent")

    def test_bootstrap_installation_detects_missing_primary_script(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = pathlib.Path(temp_dir)
            primary = root / "ardur-agent"
            worktree = primary / "dev-workspace" / "agent-bootstrap-linear"
            (worktree / "scripts").mkdir(parents=True)
            (worktree / "scripts" / "agent_bootstrap.py").write_text("# test\n", encoding="utf-8")

            installation = self.bootstrap.bootstrap_installation(worktree, primary)

        self.assertTrue(installation["repo_script_present"])
        self.assertFalse(installation["primary_script_present"])

    def test_render_markdown_warns_when_primary_bootstrap_missing(self):
        context = {
            "generated_on": "2026-06-15",
            "git": {
                "repo_root": "/Volumes/EXTENDED/ardur-agent/dev-workspace/test",
                "primary_root": "/Volumes/EXTENDED/ardur-agent",
                "branch": "gnanirahulnutakki/ARD-1-test",
                "isolated_workspace": True,
                "dirty_file_count": 0,
                "hooks_path": ".githooks",
                "worktrees": [],
            },
            "linear": {
                "available": False,
                "error": "workspace mismatch",
                "recovery_steps": ["Do not claim non-`ARD` issues from the generic Linear connector."],
            },
            "bootstrap_installation": {
                "repo_script": "/Volumes/EXTENDED/ardur-agent/dev-workspace/test/scripts/agent_bootstrap.py",
                "primary_script": "/Volumes/EXTENDED/ardur-agent/scripts/agent_bootstrap.py",
                "repo_script_present": True,
                "primary_script_present": False,
                "running_from_primary": False,
            },
            "local_facts": {
                "identity": "Ardur Agent test identity.",
                "substrate": ["cap-token authorization"],
                "e2e_stub_no_key": True,
                "known_dev_fidelity_gap": False,
            },
            "provider_memory": {
                "provider": "anthropic",
                "memory": "in_memory",
                "sensitive_inputs": {},
            },
            "local_tools": {},
            "recommended_session_journal": "/Volumes/EXTENDED/ardur-agent/architect/sessions/test/journal.md",
        }

        rendered = self.bootstrap.render_markdown(context)

        self.assertIn("not installed in the primary EXTENDED checkout", rendered)
        self.assertIn("Linear Recovery Steps", rendered)
        self.assertIn("Do not claim non-`ARD` issues", rendered)


if __name__ == "__main__":
    unittest.main()
