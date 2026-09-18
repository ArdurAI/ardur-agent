import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
TIMEOUT = '          QDRANT_TIMEOUT_SECS: "30"\n'


class QdrantCIContractTests(unittest.TestCase):
    def assert_live_contract(self, ci):
        job = re.search(
            r"(?ms)^  qdrant-integration:\n(.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)",
            ci,
        )
        if job is None:
            self.fail("the live Qdrant job must remain enabled")
        step = re.search(
            r"(?ms)^      - name: Gated Qdrant integration tests \(--ignored\)\n"
            r"(.*?)(?=^      - |\Z)",
            job.group(1),
        )
        if step is None:
            self.fail("the real integration step must remain enabled")
        env, commands = step.group(1).split("        run: |\n", 1)
        self.assertIn("        env:\n", env)
        self.assertRegex(
            env,
            r'(?m)^          QDRANT_TIMEOUT_SECS: "30"$',
            "live functional tests need an explicit bounded CI budget, not the "
            "production five-second ordinary-RPC deadline",
        )
        self.assertEqual(len(re.findall(r"(?m)^ +QDRANT_TIMEOUT_SECS:", ci)), 1)
        self.assertNotIn("QDRANT_TIMEOUT_SECS", commands, "do not override the step budget")
        self.assertNotIn("continue-on-error", job.group(1))
        self.assertNotIn("|| true", commands)
        self.assertNotIn("--test-threads", commands, "preserve parallel tests")
        self.assertNotIn("RUST_TEST_THREADS", ci.split("jobs:", 1)[0] + job.group(1))
        selected = [
            " ".join(line.split())
            for line in commands.replace("\\\n", "").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
        self.assertEqual(
            selected,
            [
                "cargo test -p ardur-memory-qdrant --test integration -- --ignored",
                "cargo test -p ardur-memory-qdrant --test hybrid_integration "
                "-- --ignored --skip semantic_hit_gated --skip hybrid_beats_either",
                "cargo test -p ardur-e2e-tests "
                "--test scenario_qdrant_memory_persistence "
                "--test scenario_hybrid_memory_full_pipeline -- --ignored",
            ],
            "preserve the live selection, including only the two semantic skips",
        )

    def test_live_tests_have_a_bounded_ci_only_rpc_deadline(self):
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        self.assert_live_contract(ci)

    def test_contract_rejects_removal_wrong_scope_and_weakened_live_checks(self):
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        self.assert_live_contract(ci)
        missing = ci.replace(TIMEOUT, "", 1)
        core = "cargo test -p ardur-memory-qdrant --test integration -- --ignored"
        mutations = {
            "missing": missing,
            "too_short": ci.replace(TIMEOUT, TIMEOUT.replace('"30"', '"5"'), 1),
            "invalid": ci.replace(TIMEOUT, TIMEOUT.replace('"30"', '"0"'), 1),
            "workflow_scope": 'env:\n  QDRANT_TIMEOUT_SECS: "30"\n' + missing,
            "duplicate_workflow_scope": 'env:\n  QDRANT_TIMEOUT_SECS: "30"\n' + ci,
            "job_scope": missing.replace(
                "  qdrant-integration:\n",
                '  qdrant-integration:\n    env:\n      QDRANT_TIMEOUT_SECS: "30"\n',
                1,
            ),
            "unrelated_step": missing.replace(
                "      - name: Wait for Qdrant readiness\n",
                "      - name: Wait for Qdrant readiness\n        env:\n" + TIMEOUT,
                1,
            ),
            "masked_failure": ci.replace(core, core + " || true", 1),
            "drop_ignored": ci.replace(
                "-- --ignored --skip semantic_hit_gated",
                "-- --skip semantic_hit_gated",
                1,
            ),
            "extra_skip": ci.replace(core, core + " --skip insert_then_query", 1),
            "serialized": ci.replace(core, core + " --test-threads=1", 1),
        }
        for name, changed in mutations.items():
            with self.subTest(name=name):
                self.assertNotEqual(changed, ci, "mutation must change the fixture")
                with self.assertRaises(AssertionError):
                    self.assert_live_contract(changed)


if __name__ == "__main__":
    unittest.main()
