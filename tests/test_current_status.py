import pathlib
import re
import subprocess
import tempfile
import textwrap
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]


class CurrentStatusTests(unittest.TestCase):
    def test_workspace_inventory_matches_manifest(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        status = (ROOT / "docs/current-status.md").read_text(encoding="utf-8")
        counts = re.findall(r"Cargo metadata reports (\d+) workspace packages\.", status)
        self.assertEqual(len(counts), 1, "declare the workspace inventory exactly once")
        self.assertEqual(
            int(counts[0]),
            len(manifest["workspace"]["members"]),
            "re-baseline the status inventory when workspace membership changes",
        )

    def test_offline_recipe_neutralizes_inherited_provider_and_key(self):
        status = (ROOT / "docs/current-status.md").read_text(encoding="utf-8")
        snippets = [
            body
            for body in re.findall(r"^```(?:sh|bash)\n(.*?)\n```", status, re.M | re.S)
            if "ardur chat --plain" in body
        ]
        self.assertEqual(len(snippets), 1, "document exactly one executable offline CLI recipe")
        with tempfile.TemporaryDirectory() as temp_dir:
            fake = pathlib.Path(temp_dir) / "ardur"
            fake.write_text(
                textwrap.dedent("""\
                    #!/bin/sh
                    printf 'command=%s\\n' "$*"
                    selected=missing
                    [ "${ARDUR_PROVIDER-}" != anthropic ] || selected=present
                    key=missing
                    [ "${ANTHROPIC_API_KEY+x}" != x ] || key=present
                    printf 'default-provider=%s\\nanthropic-key=%s\\n' "$selected" "$key"
                    """),
                encoding="utf-8",
            )
            fake.chmod(0o755)
            # No ambient credentials, real CLI, or toolchain is reachable via PATH.
            result = subprocess.run(
                ["/bin/sh", "-eu", "-c", snippets[0]],
                cwd=temp_dir,
                env={
                    "PATH": temp_dir,
                    "HOME": temp_dir,
                    "ARDUR_PROVIDER": "ollama",
                    "ANTHROPIC_API_KEY": "inherited-test-only",
                },
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                "command=setup --yes", "default-provider=present", "anthropic-key=missing",
                "command=chat --plain", "default-provider=present", "anthropic-key=missing",
            ],
            "the offline recipe must override the provider and remove even an inherited key",
        )


if __name__ == "__main__":
    unittest.main()
