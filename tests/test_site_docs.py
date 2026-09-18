import importlib.util
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "site-docs.py"
SITE_DEPLOY = ROOT / ".github" / "workflows" / "site-deploy.yml"


def load_generator():
    spec = importlib.util.spec_from_file_location("site_docs", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class AllowlistGuardTests(unittest.TestCase):
    """Local-only paths must be impossible to publish (#515 S1, F2)."""

    def setUp(self):
        self.gen = load_generator()

    def test_forbidden_paths_are_rejected(self):
        forbidden = [
            ".context/notes.md",
            "architect/plans/plan.md",
            "architect/sessions/2026-01-01-x/journal.md",
            "plans/internal.md",
            "dev-workspace/foo/scratch.md",
            "handoff/bundle.md",
            "docs/2026-09-17-site-s0-session-journal.md",
            "architect/sessions/2026-09-17-x/session-notes.md",
            "EXTENDED/ardur-agent/secret.md",
            "/Volumes/EXTENDED/x.md",
        ]
        for path in forbidden:
            self.assertTrue(
                self.gen.is_forbidden(path),
                f"{path!r} must be rejected by the pattern guard",
            )
        # Product docs that merely mention sessions must stay publishable.
        self.assertFalse(self.gen.is_forbidden("docs/session-lifecycle.md"))

    def test_allowlisted_docs_pass_the_guard(self):
        for src in self.gen.ALLOWED_DOCS:
            self.assertFalse(self.gen.is_forbidden(src), src)
            self.assertTrue((ROOT / src).is_file(), f"allowlisted doc missing: {src}")

    def test_internal_working_docs_are_not_allowlisted(self):
        """Internal-lane documents stay repo-only by editorial choice."""
        not_published = [
            "docs/agents/session-code-of-conduct.md",
            "docs/cli/cli-competitive-survey.md",
            "docs/cli/cli-stunning-design.md",
            "docs/cli/decisions/ADR-Phase2-021-stunning-cli.md",
            "docs/roadmap/high-assurance-personal-agent-roadmap-2026-06-16.md",
        ]
        for path in not_published:
            self.assertNotIn(path, self.gen.ALLOWED_DOCS)
            self.assertTrue((ROOT / path).is_file(), f"fixture moved: {path}")

    def test_generation_is_deterministic(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            # Minimal fake repo with just the allowlisted sources.
            for src in self.gen.ALLOWED_DOCS:
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(
                    f"# Title {src}\n\nFirst paragraph for {src}.\n\n## Section\n\nBody.\n",
                    encoding="utf-8",
                )
            first = self.gen.generate(repo)
            files_first = {
                p.name: p.read_text(encoding="utf-8")
                for p in (repo / self.gen.OUTPUT_DIR).glob("*.md")
            }
            second = self.gen.generate(repo)
            files_second = {
                p.name: p.read_text(encoding="utf-8")
                for p in (repo / self.gen.OUTPUT_DIR).glob("*.md")
            }
            self.assertEqual(first, second)
            self.assertEqual(files_first, files_second, "generation must be deterministic")
            # Frontmatter injected, H1 stripped, header present.
            sample = files_first["benchmarks.md"]
            self.assertIn('title: "Title docs/benchmarks.md"', sample)
            self.assertIn("layout: docs", sample)
            self.assertIn("do not hand-edit", sample)
            self.assertNotIn("# Title docs/benchmarks.md", sample)
            self.assertIn("First paragraph for", sample)

    def test_missing_allowlisted_doc_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            with self.assertRaises(self.gen.DocsError):
                self.gen.generate(repo)

    def test_symlinked_allowlist_entry_is_rejected(self):
        """A symlink at an allowlisted path must not publish its target (#528)."""
        import os

        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            secret = repo / "architect" / "private.md"
            secret.parent.mkdir(parents=True)
            secret.write_text("# Secret\n\nInternal.\n", encoding="utf-8")
            doc = repo / "docs" / "benchmarks.md"
            doc.parent.mkdir(parents=True)
            os.symlink(secret, doc)
            for src in self.gen.ALLOWED_DOCS:
                if src == "docs/benchmarks.md":
                    continue
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(f"# T {src}\n\nD {src}.\n", encoding="utf-8")
            with self.assertRaises(self.gen.DocsError):
                self.gen.generate(repo)

    def test_yaml_scalars_survive_quotes_and_backslashes(self):
        self.assertEqual(
            self.gen.yaml_scalar('Ardur "Quick Start"'),
            '"Ardur \\"Quick Start\\""',
        )
        self.assertEqual(self.gen.yaml_scalar("back\\slash"), '"back\\\\slash"')

    def test_relative_links_are_rewritten(self):
        """Cross-doc links go to slugs; repo files to GitHub URLs (#528)."""
        gen = self.gen
        self.assertEqual(
            gen.rewrite_link("fresh-machine.md", "docs/current-status.md"),
            "../fresh-machine/",
        )
        self.assertEqual(
            gen.rewrite_link("../RUN.md", "docs/fresh-machine.md"),
            "https://github.com/ArdurAI/ardur-agent/blob/dev/RUN.md",
        )
        self.assertEqual(
            gen.rewrite_link("https://example.com/x", "docs/a.md"),
            "https://example.com/x",
        )
        self.assertEqual(gen.rewrite_link("#anchor", "docs/a.md"), "#anchor")
        self.assertEqual(
            gen.rewrite_link("../crates/benches/README.md", "docs/benchmarks.md"),
            "https://github.com/ArdurAI/ardur-agent/blob/dev/crates/benches/README.md",
        )

    def test_generated_body_contains_rewritten_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            body = (
                "# Title\n\nSee [the runbook](fresh-machine.md) and "
                "[RUN.md](../RUN.md).\n"
            )
            for src in ("docs/benchmarks.md", "docs/fresh-machine.md"):
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                content = body if src.endswith("benchmarks.md") else "# FM\n\nFM body.\n"
                target.write_text(content, encoding="utf-8")
            for src in self.gen.ALLOWED_DOCS:
                if src in ("docs/benchmarks.md", "docs/fresh-machine.md"):
                    continue
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(f"# T {src}\n\nD {src}.\n", encoding="utf-8")
            self.gen.generate(repo)
            generated = (repo / self.gen.OUTPUT_DIR / "benchmarks.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("(../fresh-machine/)", generated)
            self.assertIn(
                "(https://github.com/ArdurAI/ardur-agent/blob/dev/RUN.md)", generated
            )

    def test_links_inside_code_fences_are_not_rewritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            doc = repo / "docs" / "benchmarks.md"
            doc.parent.mkdir(parents=True)
            doc.write_text(
                "# T\n\n```\nsee [x](fresh-machine.md)\n```\n",
                encoding="utf-8",
            )
            for src in self.gen.ALLOWED_DOCS:
                if src == "docs/benchmarks.md":
                    continue
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(f"# T {src}\n\nD {src}.\n", encoding="utf-8")
            self.gen.generate(repo)
            generated = (repo / self.gen.OUTPUT_DIR / "benchmarks.md").read_text(
                encoding="utf-8"
            )
            self.assertIn("[x](fresh-machine.md)", generated)

    def test_footer_links_docs_and_status_on_all_viewports(self):
        footer = (
            ROOT / "site" / "layouts" / "partials" / "footer.html"
        ).read_text(encoding="utf-8")
        self.assertIn('href="{{ "docs/" | relURL }}"', footer)
        self.assertIn('href="{{ "status/" | relURL }}"', footer)
        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            for src in self.gen.ALLOWED_DOCS:
                target = repo / src
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(f"# T {src}\n\nD {src}.\n", encoding="utf-8")
            self.gen.generate(repo)
            self.assertTrue(self.gen.check(repo), "fresh generation must match itself")
            page = next((repo / self.gen.OUTPUT_DIR).glob("*.md"))
            page.write_text("hand-edited\n", encoding="utf-8")
            self.assertFalse(self.gen.check(repo), "hand-edits must be detected")


class WorkflowWiringTests(unittest.TestCase):
    def test_docs_generation_step_precedes_build(self):
        workflow = SITE_DEPLOY.read_text(encoding="utf-8")
        docs = workflow.index("Generate docs pages")
        build = workflow.index("- name: Build")
        self.assertLess(docs, build)

    def test_generated_docs_are_gitignored(self):
        gitignore = (ROOT / "site" / ".gitignore").read_text(encoding="utf-8")
        self.assertIn("content/docs/*", gitignore)

    def test_docs_section_index_is_committed(self):
        self.assertTrue((ROOT / "site" / "content" / "docs" / "_index.md").is_file())


if __name__ == "__main__":
    unittest.main()
