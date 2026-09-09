"""Guard the PWA SSE parser against the fused-runtime event contract."""

from __future__ import annotations

import pathlib
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SSE_JS = ROOT / "web-client" / "sse.js"
APP_JS = ROOT / "web-client" / "app.js"


class WebClientSseTests(unittest.TestCase):
    def test_node_sse_parser_suite(self) -> None:
        result = subprocess.run(
            ["node", "--test", "web-client/sse.test.js"],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_app_uses_content_only_helper(self) -> None:
        app = APP_JS.read_text(encoding="utf-8")
        sse = SSE_JS.read_text(encoding="utf-8")
        self.assertIn("from './sse.js'", app)
        self.assertIn("contentTextFromPayload", app)
        self.assertNotIn("parsed.delta", app)
        self.assertIn('type === \'content\'', sse.replace('"', "'"))


if __name__ == "__main__":
    unittest.main()
