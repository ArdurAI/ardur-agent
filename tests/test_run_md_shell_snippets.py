import pathlib
import re
import subprocess
import textwrap
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
RUN_MD = ROOT / "RUN.md"
FENCE_RE = re.compile(
    r"^[ \t]*```(?:sh|bash)\n(?P<body>.*?)(?:\n^[ \t]*```)",
    re.MULTILINE | re.DOTALL,
)


def run_md_shell_block_containing(marker: str) -> str:
    text = RUN_MD.read_text(encoding="utf-8")
    matches = [
        textwrap.dedent(match.group("body")).strip("\n")
        for match in FENCE_RE.finditer(text)
        if marker in match.group("body")
    ]
    if len(matches) != 1:
        raise AssertionError(f"expected exactly one RUN.md shell block containing {marker!r}")
    return matches[0]


def run_with_bash(script: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash"],
        cwd=ROOT,
        input=script,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


class RunMdShellSnippetTests(unittest.TestCase):
    def test_mcp_curl_quick_check_is_copy_paste_safe(self):
        snippet = run_md_shell_block_containing("http://localhost:3000/mcp/ardur")
        result = run_with_bash(
            textwrap.dedent(
                f"""
                set -euo pipefail
                curl() {{ printf '%s\\n' "$@"; }}
                ARDUR_MCP_TOKEN=token
                {snippet}
                """
            )
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Authorization: Bearer token", result.stdout)
        self.assertIn("Content-Type: application/json", result.stdout)

    def test_ardur_admin_example_is_one_copy_paste_safe_command(self):
        snippet = run_md_shell_block_containing("ardur-admin")
        result = run_with_bash(
            textwrap.dedent(
                f"""
                set -euo pipefail
                ardur-admin() {{ printf '%s\\n' "$@"; }}
                {snippet}
                """
            )
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--journal-dir", result.stdout)
        self.assertIn("--qdrant-url", result.stdout)
        self.assertIn("--port", result.stdout)
        self.assertIn("--basic-auth", result.stdout)


if __name__ == "__main__":
    unittest.main()
