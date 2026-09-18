import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "site-progress.py"
SITE_DEPLOY = ROOT / ".github" / "workflows" / "site-deploy.yml"


def load_collector():
    spec = importlib.util.spec_from_file_location("site_progress", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ParseTestResultLinesTests(unittest.TestCase):
    def setUp(self):
        self.collector = load_collector()

    def test_sums_libtest_lines(self):
        log = "\n".join(
            [
                "2026-01-01T00:00:00Z running 3 tests",
                "2026-01-01T00:00:01Z test result: ok. 10 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.01s",
                "2026-01-01T00:00:02Z test result: ok. 5 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s",
                "2026-01-01T00:00:03Z some other line",
            ]
        )
        result = self.collector.parse_test_result_lines(log)
        self.assertEqual(result["passed"], 15)
        self.assertEqual(result["failed"], 1)
        self.assertEqual(result["ignored"], 2)
        self.assertEqual(result["seen"], 2)

    def test_no_result_lines_reports_zero_seen(self):
        result = self.collector.parse_test_result_lines("nothing here\n")
        self.assertEqual(result["seen"], 0)
        self.assertEqual(result["passed"], 0)


class BeadsTests(unittest.TestCase):
    def setUp(self):
        self.collector = load_collector()

    def test_entries_drop_finished_items_and_allowlist_fields(self):
        items = [
            {
                "id": "ardur-agent-1",
                "title": "Active work",
                "status": "open",
                "description": "SECRET-ADJACENT BODY must not leak",
            },
            {
                "id": "ardur-agent-2",
                "title": "Merged work",
                "status": "closed",
                "description": "done",
            },
            {"id": "ardur-agent-3", "title": "In progress", "status": "in_progress"},
        ]
        entries = self.collector.beads_to_entries(items)
        self.assertEqual(len(entries), 2)
        self.assertEqual(
            {e["source_sha_or_id"] for e in entries},
            {"ardur-agent-1", "ardur-agent-3"},
        )
        for e in entries:
            self.assertEqual(e["source"], "beads")
            self.assertIsNone(e["evidence_url"])
        self.assertNotIn("description", entries[0])
        self.assertNotIn("SECRET-ADJACENT BODY", json.dumps(entries))

    def test_snapshot_round_trip_is_allowlisted_and_marked_stale(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo_root = pathlib.Path(tmp)
            raw = [
                {
                    "id": "x-1",
                    "title": "T",
                    "status": "open",
                    "notes": "private note that must not ship",
                }
            ]
            self.collector.write_beads_snapshot(repo_root, raw)
            loaded = self.collector.load_beads_snapshot(repo_root)
            self.assertEqual(loaded["items"], [{"id": "x-1", "title": "T", "status": "open"}])
            self.assertIn("do not hand-edit", loaded["_note"])

            rows, meta = self.collector.collect_beads(repo_root)
            # bd may exist on the dev machine; force the snapshot path by
            # checking the meta contract both ways.
            if not meta["fresh"]:
                self.assertEqual(rows, loaded["items"])
                self.assertFalse(meta["fresh"])
                self.assertEqual(meta["regenerated_at"], loaded["regenerated_at"])

    def test_missing_bd_and_snapshot_is_fatal(self):
        from unittest import mock

        with tempfile.TemporaryDirectory() as tmp:
            repo_root = pathlib.Path(tmp)
            with mock.patch("shutil.which", return_value=None):
                with self.assertRaises(self.collector.CollectorError):
                    self.collector.collect_beads(repo_root)

    def test_bd_failure_without_snapshot_degrades_to_fatal(self):
        """bd installed but no local .beads and no snapshot: fatal, not empty."""
        from unittest import mock

        def fake_run(cmd, cwd=None):
            raise self.collector.CollectorError("no beads database found")

        with tempfile.TemporaryDirectory() as tmp:
            repo_root = pathlib.Path(tmp)
            with mock.patch("shutil.which", return_value="/usr/local/bin/bd"):
                with mock.patch.object(self.collector, "run", side_effect=fake_run):
                    with self.assertRaises(self.collector.CollectorError):
                        self.collector.collect_beads(repo_root)

    def test_bd_failure_with_snapshot_degrades_marked_stale(self):
        """bd installed, no local .beads, committed snapshot present: stale carry."""
        from unittest import mock

        def fake_run(cmd, cwd=None):
            raise self.collector.CollectorError("no beads database found")

        with tempfile.TemporaryDirectory() as tmp:
            repo_root = pathlib.Path(tmp)
            self.collector.write_beads_snapshot(repo_root, [])
            with mock.patch("shutil.which", return_value="/usr/local/bin/bd"):
                with mock.patch.object(self.collector, "run", side_effect=fake_run):
                    rows, meta = self.collector.collect_beads(repo_root)
            self.assertFalse(meta["fresh"])
            self.assertEqual(rows, [])
            self.assertIn("carried-forward", meta["note"])

    def test_bd_failure_note_carries_no_command_diagnostics(self):
        """Diagnostics (paths, config detail) must stay out of metadata (#526 P1).

        run() embeds command stderr into CollectorError; none of it may
        reach the generated public metadata — fixed-string reasons only.
        """
        from unittest import mock

        def fake_run(cmd, cwd=None):
            raise self.collector.CollectorError(
                "command failed (1): bd list --json\nError: /Users/x/.beads secret"
            )

        with tempfile.TemporaryDirectory() as tmp:
            repo_root = pathlib.Path(tmp)
            self.collector.write_beads_snapshot(repo_root, [])
            with mock.patch("shutil.which", return_value="/usr/local/bin/bd"):
                with mock.patch.object(self.collector, "run", side_effect=fake_run):
                    _, meta = self.collector.collect_beads(repo_root)
            self.assertNotIn("/Users/x", json.dumps(meta))
            self.assertNotIn(".beads secret", json.dumps(meta))


class StrictModeTests(unittest.TestCase):
    def test_strict_exits_one_on_missing_sources(self):
        with tempfile.TemporaryDirectory() as tmp:
            # A directory with no git metadata cannot yield a repo slug, so
            # collection fails before any network access.
            proc = subprocess.run(
                [
                    "python3",
                    str(SCRIPT),
                    "--repo-root",
                    tmp,
                    "--strict",
                ],
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(proc.returncode, 1, proc.stderr)
            self.assertIn("error:", proc.stderr)


class WorkflowWiringTests(unittest.TestCase):
    """Guard the site-deploy generation step (plan #515 S0)."""

    def setUp(self):
        self.collector = load_collector()

    def test_regeneration_step_precedes_hugo_build(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        regen = workflow.index("Regenerate status data")
        build = workflow.index("- name: Build")
        self.assertLess(regen, build)

    def test_regeneration_uses_strict_mode_on_pushes(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        self.assertIn("site-progress.py --strict", workflow)
        # Loud failure on pushes; PR validations only warn so fork PRs
        # without repo access don't break the required hugo check.
        self.assertIn(
            "continue-on-error: ${{ github.event_name == 'pull_request' }}", workflow
        )

    def test_nightly_schedule_present(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        self.assertRegex(workflow, r"(?s)schedule:.*?- cron: \"40 2 \* \* \*\"")

    def test_workflow_grants_actions_read_for_job_logs(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        self.assertLess(
            workflow.index("actions: read"), workflow.index("jobs:"), "permissions must precede jobs"
        )

    def test_scheduled_runs_deploy(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        self.assertEqual(
            workflow.count("github.event_name == 'schedule'"), 2,
            "both upload and deploy must include scheduled runs",
        )

    def test_release_collection_failure_propagates_in_strict_mode(self):
        from unittest import mock

        def fail(slug):
            raise self.collector.CollectorError("HTTP 403 rate limited")

        with mock.patch.object(self.collector, "gh_api", side_effect=fail):
            with self.assertRaises(self.collector.CollectorError):
                self.collector.collect_release("x/y")

    def test_dev_head_is_ci_run_head_not_checkout(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        self.assertNotIn("git rev-parse", workflow)

    def test_progress_data_is_tracked_so_pr_builds_render(self):
        data = ROOT / "site" / "data" / "progress.json"
        self.assertTrue(data.is_file(), "committed progress.json must exist")
        payload = json.loads(data.read_text(encoding="utf-8"))
        self.assertIn("generated_by", payload["_meta"])
        self.assertIn("do not hand-edit", payload["_meta"]["note"])
        for required in ("entries", "summary", "sources", "generated_at"):
            self.assertIn(required, payload)
        for entry in payload["entries"]:
            for field in ("title", "lane", "state", "evidence_url", "source_sha_or_id", "updated_at"):
                self.assertIn(field, entry)

    def test_status_page_exists_and_links_regeneration_time(self):
        content = ROOT / "site" / "content" / "status.md"
        layout = ROOT / "site" / "layouts" / "status.html"
        self.assertTrue(content.is_file())
        self.assertTrue(layout.is_file())
        html = layout.read_text(encoding="utf-8")
        self.assertIn("Last regenerated", html)
        self.assertIn("site.Data.progress", html)


if __name__ == "__main__":
    unittest.main()
