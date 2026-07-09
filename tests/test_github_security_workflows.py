import json
import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
PINNED_ACTION_RE = re.compile(r"uses:\s+[^\s#]+@(?P<ref>[0-9a-f]{40})(?:\s+#\s+\S+)?")
TAGGED_ACTION_RE = re.compile(r"uses:\s+[^\s#]+@(?![0-9a-f]{40}\b)(?P<ref>[^\s#]+)")


class GitHubSecurityWorkflowTests(unittest.TestCase):
    def test_cargo_deny_action_receives_categories_as_command_arguments(self):
        ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")

        self.assertIn("arguments: --workspace", ci)
        self.assertIn("command-arguments: advisories bans licenses sources", ci)
        self.assertNotIn("arguments: --workspace check advisories", ci)
        self.assertNotIn("arguments: --workspace advisories bans licenses sources", ci)

    def test_workflows_declare_minimal_top_level_permissions(self):
        for path in WORKFLOWS.glob("*.yml"):
            workflow = path.read_text(encoding="utf-8")
            self.assertRegex(workflow, r"(?m)^permissions:\n(?:  [a-z-]+: [a-z-]+\n)+", path.name)
            self.assertLess(workflow.index("permissions:"), workflow.index("jobs:"), path.name)

    def test_actions_are_pinned_to_full_commit_shas(self):
        for path in WORKFLOWS.glob("*.yml"):
            workflow = path.read_text(encoding="utf-8")
            for match in re.finditer(r"(?m)^\s*-?\s*uses:\s+([^\s#]+)", workflow):
                spec = match.group(1)
                if spec.startswith("./"):
                    continue
                self.assertRegex(
                    match.group(0),
                    PINNED_ACTION_RE,
                    f"{path.name} must pin {spec} to a 40-char commit SHA",
                )
                self.assertIsNone(TAGGED_ACTION_RE.search(match.group(0)))

    def test_ci_runs_codeql_and_dependency_review_security_gates(self):
        ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")

        self.assertIn("name: CodeQL / Rust", ci)
        self.assertIn(
            "github/codeql-action/init@99df26d4f13ea111d4ec1a7dddef6063f76b97e9",
            ci,
        )
        self.assertIn(
            "github/codeql-action/analyze@99df26d4f13ea111d4ec1a7dddef6063f76b97e9",
            ci,
        )
        self.assertIn("security-events: write", ci)
        self.assertIn("languages: rust", ci)
        self.assertIn("build-mode: none", ci)
        self.assertNotIn("build-mode: manual", ci)

        self.assertIn("name: dependency-review", ci)
        self.assertIn("if: github.event_name == 'pull_request'", ci)
        self.assertIn("pull-requests: read", ci)
        self.assertIn(
            "actions/dependency-review-action@2031cfc080254a8a887f58cffee85186f0e49e48",
            ci,
        )
        self.assertIn("fail-on-severity: high", ci)

    def test_site_workflow_validates_main_and_dev_but_deploys_only_main(self):
        site = (WORKFLOWS / "site-deploy.yml").read_text(encoding="utf-8")

        self.assertRegex(site, r"push:\n\s+branches: \[main, dev\]")
        self.assertRegex(site, r"pull_request:\n\s+branches: \[main, dev\]")
        self.assertNotIn("paths:", site)
        self.assertIn("hugo:\n    name: hugo", site)
        self.assertIn("run: npm ci", site)
        self.assertTrue((ROOT / "site" / "package-lock.json").is_file())
        self.assertIn("needs: hugo", site)
        self.assertIn("github.ref == 'refs/heads/main'", site)

    def test_ruleset_required_checks_match_emitted_workflow_job_names(self):
        ruleset = json.loads(
            (ROOT / ".github" / "rulesets" / "main-dev-security-gates.json").read_text(
                encoding="utf-8"
            )
        )
        contexts = {
            check["context"]
            for rule in ruleset["rules"]
            if rule["type"] == "required_status_checks"
            for check in rule["parameters"]["required_status_checks"]
        }

        self.assertEqual(
            contexts,
            {
                "cargo-deny (advisories + licenses + bans)",
                "dependency-review",
                "CodeQL / Rust",
                "ubuntu-latest / stable",
                "macos-15 / stable",
                "hugo",
                "build-healthcheck-scan",
                "Check DCO sign-off",
            },
        )
        self.assertFalse(any(re.search(r"rust /|site /|docker /|security /", c) for c in contexts))

    def test_rust_toolchain_is_exactly_pinned(self):
        toolchain = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")

        self.assertIn('channel = "1.96.1"', toolchain)
        self.assertNotIn('channel = "stable"', toolchain)


if __name__ == "__main__":
    unittest.main()
