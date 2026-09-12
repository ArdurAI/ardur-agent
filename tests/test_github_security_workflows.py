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
        # Assert codeql-action is pinned to a 40-char SHA (any version).
        # The pinning invariant matters, not the specific digest — dependabot
        # bumps the version and this test must not need a manual update.
        self.assertRegex(
            ci,
            r"github/codeql-action/init@[0-9a-f]{40}",
            "codeql-action/init must be pinned to a 40-char commit SHA",
        )
        self.assertRegex(
            ci,
            r"github/codeql-action/analyze@[0-9a-f]{40}",
            "codeql-action/analyze must be pinned to a 40-char commit SHA",
        )
        self.assertIn("security-events: write", ci)
        self.assertIn("languages: rust", ci)
        self.assertIn("build-mode: none", ci)
        self.assertNotIn("build-mode: manual", ci)

        self.assertIn("name: dependency-review", ci)
        self.assertIn("if: github.event_name == 'pull_request'", ci)
        self.assertIn("pull-requests: read", ci)
        self.assertIn(
            "actions/dependency-review-action@a1d282b36b6f3519aa1f3fc636f609c47dddb294",
            ci,
        )
        self.assertIn("fail-on-severity: high", ci)

    def test_release_workflow_uploads_sbom_signed_assets_and_provenance(self):
        release_path = WORKFLOWS / "release.yml"
        self.assertTrue(release_path.is_file(), "release workflow must exist")
        release = release_path.read_text(encoding="utf-8")

        self.assertIn("name: release-supply-chain", release)
        self.assertIn("release:\n    types: [published]", release)
        self.assertNotIn("pull_request:", release)

        self.assertIn("contents: write", release)
        self.assertIn("id-token: write", release)
        self.assertIn("attestations: write", release)
        self.assertNotIn("security-events: write", release)

        self.assertIn("cargo build --workspace --bins --release --locked", release)
        self.assertIn("anchore/sbom-action@3ad7283483fc7af8ff2b4ea19663c2d5ca935e26", release)
        self.assertIn("format: spdx-json", release)
        self.assertIn("output-file: dist/ardur-agent-${{ env.RELEASE_TAG }}.spdx.json", release)
        self.assertIn("sha256sum", release)
        self.assertIn("SHA256SUMS", release)

        self.assertIn("actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8", release)
        self.assertIn("subject-path: dist/*", release)

        self.assertIn("sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6", release)
        self.assertIn("cosign sign-blob --yes", release)
        self.assertIn("--output-signature", release)
        self.assertIn("--output-certificate", release)
        self.assertIn("--bundle", release)

        self.assertIn("gh release upload", release)
        self.assertIn("--clobber", release)

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

    def test_docker_workflow_publishes_ghcr_on_version_tags(self):
        docker = (WORKFLOWS / "docker.yml").read_text(encoding="utf-8")
        header, jobs = docker.split("jobs:", 1)

        self.assertIn('tags: ["v*"]', header)
        self.assertNotIn("packages: write", header)
        self.assertIn("packages: write", jobs)
        self.assertNotIn("name: publish-ghcr", docker)
        self.assertNotIn(":latest", docker)
        self.assertNotIn("push: true", docker)
        self.assertIn("docker tag ardur-agent:ci", jobs)
        self.assertIn("docker push", jobs)
        self.assertIn("merge-base --is-ancestor", jobs)
        self.assertIn(":sha-${GITHUB_SHA}", jobs)
        self.assertIn("Promote attested image to the version tag", jobs)
        tag_if = (
            "if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"
        )
        self.assertEqual(jobs.count(tag_if), 5, jobs)
        self.assertIn(tag_if, jobs)
        self.assertIn(
            "docker/login-action@dbcb813823bdd20940b903addbd779551569679f",
            jobs,
        )
        self.assertIn("registry: ghcr.io", jobs)
        self.assertIn("ghcr.io/${repo}", jobs)
        self.assertIn("subject-name: ${{ steps.publish.outputs.image }}", jobs)
        self.assertIn("subject-digest: ${{ steps.publish.outputs.digest }}", jobs)

    def test_dco_reads_exempt_list_from_the_base_ref(self):
        """The exempt list must come from base, never the PR head.

        Regression guard for #435: dco.yml read .github/dco-exempt-shas.txt
        from the checked-out merge ref, so a PR could append its own unsigned
        SHA and self-exempt. Reading it from the base ref means an exemption
        only takes effect after it has been reviewed and merged.
        """
        dco = (WORKFLOWS / "dco.yml").read_text(encoding="utf-8")

        # The applied list is read out of the base commit...
        self.assertIn('EXEMPT_RAW="$(git show "${BASE}:${EXEMPT_FILE}")"', dco)
        # ...and never straight off the working tree.
        self.assertNotIn(
            "grep -vE '^[[:space:]]*(#|$)' .github/dco-exempt-shas.txt",
            dco,
            "the exempt list must not be read from the checked-out head (#435)",
        )
        # A missing file on base must mean "no exemptions", not "skip the check".
        self.assertIn('EXEMPT_RAW=""', dco)

    def test_dco_enforces_append_only_and_full_shas_for_new_exemptions(self):
        """New exemptions must be full SHAs and may not rewrite history."""
        dco = (WORKFLOWS / "dco.yml").read_text(encoding="utf-8")

        self.assertIn("append-only", dco)
        self.assertRegex(
            dco,
            r"\^\[0-9a-f\]\{40\}\$",
            "new exempt entries must be required to be full 40-char SHAs",
        )
        self.assertRegex(
            dco,
            r"\^\[0-9a-f\]\{7,40\}\$",
            "applied exempt entries must be validated as lowercase hex",
        )

    def test_dco_exempt_file_entries_are_hex_shas(self):
        """The committed exempt list itself must contain only hex SHAs."""
        raw = (ROOT / ".github" / "dco-exempt-shas.txt").read_text(encoding="utf-8")

        entries = [
            line.split()[0]
            for line in raw.splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
        self.assertTrue(entries, "exempt file should not be empty")
        for entry in entries:
            self.assertRegex(
                entry,
                r"^[0-9a-f]{7,40}$",
                f"exempt entry {entry!r} must be 7-40 lowercase hex characters",
            )

    def test_codeowners_guards_the_exemption_policy_files(self):
        """Files that waive security requirements stay owner-reviewed (#435)."""
        codeowners = (ROOT / ".github" / "CODEOWNERS").read_text(encoding="utf-8")

        for guarded in (
            "/.github/dco-exempt-shas.txt",
            "/.github/workflows/dco.yml",
            "/.gitleaksignore",
        ):
            self.assertIn(guarded, codeowners, f"{guarded} must have an explicit owner")

    def test_rust_toolchain_is_exactly_pinned(self):
        toolchain = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")

        self.assertIn('channel = "1.98.1"', toolchain)
        self.assertNotIn('channel = "stable"', toolchain)

    def test_dockerfile_builder_matches_rust_toolchain_channel(self):
        """The builder image must be the same rustc patch release as CI.

        Regression guard for #436: the builder was pinned to a `1.98-slim`
        minor tag, which floated to rustc 1.98.0 while rust-toolchain.toml
        pinned 1.98.1 — CI-validated artifacts and the released image were
        built by different compilers. Asserting on the *tag* (not a digest
        lookup) keeps this test hermetic: no registry access, and a digest
        bump that silently changes the patch version still fails here.
        """
        toolchain = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
        dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")

        channel_match = re.search(
            r'(?m)^\s*channel\s*=\s*"(?P<channel>[0-9]+\.[0-9]+\.[0-9]+)"', toolchain
        )
        if channel_match is None:
            self.fail("rust-toolchain.toml must pin an exact x.y.z channel")
        channel = channel_match.group("channel")

        builder_match = re.search(
            r"(?m)^FROM\s+rust:(?P<tag>[^@\s]+)@(?P<digest>sha256:[0-9a-f]{64})\s+AS\s+builder",
            dockerfile,
        )
        if builder_match is None:
            self.fail(
                "Dockerfile builder must be `FROM rust:<tag>@sha256:<digest> AS builder`"
            )

        tag = builder_match.group("tag")
        self.assertTrue(
            tag.startswith(f"{channel}-"),
            f"Dockerfile builder tag {tag!r} must carry the full toolchain patch "
            f"version {channel!r} (e.g. {channel}-slim); a floating minor tag "
            f"builds releases with a different rustc than CI (#436)",
        )


if __name__ == "__main__":
    unittest.main()
