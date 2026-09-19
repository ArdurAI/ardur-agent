"""Release-only guards; fixtures are not evidence of a real signed build."""

import hashlib
import importlib.util
import pathlib
import re
import struct
import subprocess
import tarfile
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/release.yml"
TARGET_RUNNERS = {
    "x86_64-unknown-linux-gnu": "ubuntu-latest",
    "aarch64-unknown-linux-gnu": "ubuntu-24.04-arm",
    "x86_64-apple-darwin": "macos-15-intel",
    "aarch64-apple-darwin": "macos-15",
}


class ReleaseMatrixWorkflowTests(unittest.TestCase):
    def test_four_native_lanes_share_pinned_build_and_packaged_smoke(self):
        workflow = WORKFLOW.read_text()
        lanes = re.findall(r"- target: (\S+)\n\s+os: (\S+)", workflow)
        self.assertEqual(dict(lanes), TARGET_RUNNERS)
        self.assertEqual(len(lanes), 4)
        build, aggregate = workflow.split("  release-artifacts:\n", 1)
        self.assertIn("runs-on: ${{ matrix.os }}", build)
        self.assertIn("fail-fast: false", build)
        self.assertIn("permissions:\n      contents: read", build)
        self.assertNotIn("id-token: write", build)
        self.assertIn("RUSTUP_TOOLCHAIN: 1.98.1", build)
        self.assertIn('rustup toolchain install 1.98.1 --profile minimal --target "${TARGET}"', build)
        self.assertIn('cargo build --workspace --bins --release --locked --target "${TARGET}"', build)
        self.assertIn('test "$(rustc -vV |', build)
        self.assertIn('tar -xzf "dist/ardur-${RELEASE_TAG}-${TARGET}.tar.gz" -C smoke', build)
        self.assertIn("./smoke/ardur --version", build)
        self.assertIn("if-no-files-found: error", build)
        self.assertIn("needs: build-release", aggregate)
        self.assertNotIn("if: always()", aggregate)
        self.assertNotIn("continue-on-error", workflow)

    def test_documented_targets_match_the_built_matrix(self):
        documented = re.findall(r"(?m)^\| `([^`]+)` \| `([^`]+)` \|", (ROOT / "docs/releases.md").read_text())
        self.assertEqual(dict(documented), TARGET_RUNNERS)
        self.assertEqual(len(documented), len(TARGET_RUNNERS))

    def test_single_aggregate_signs_and_verifies_before_publication(self):
        workflow = WORKFLOW.read_text()
        aggregate = workflow.split("  release-artifacts:\n", 1)[1]
        self.assertIn("merge-multiple: true", aggregate)
        self.assertIn("upload-release-assets: false", aggregate)
        self.assertEqual(workflow.count("uses: anchore/sbom-action@"), 1)
        self.assertEqual(workflow.count("name: Generate SHA256SUMS"), 1)
        self.assertIn('sha256sum -- *.tar.gz *.spdx.json > SHA256SUMS', aggregate)
        self.assertIn("release_assets=( *.tar.gz *.spdx.json SHA256SUMS )", aggregate)
        self.assertIn("cosign verify-blob", aggregate)
        self.assertIn('--certificate-identity "https://github.com/${GITHUB_WORKFLOW_REF}"', aggregate)
        self.assertIn("https://token.actions.githubusercontent.com", aggregate)
        self.assertIn("sha256sum --check SHA256SUMS", aggregate)
        markers = [
            "--stage archives", "name: Generate release SPDX SBOM",
            "--stage unsigned", "name: Generate SHA256SUMS",
            "name: Attest release artifact provenance",
            "name: Sign release assets with keyless cosign", "--stage signed",
            "cosign verify-blob", "gh release upload",
        ]
        positions = [aggregate.index(marker) for marker in markers]
        self.assertEqual(positions, sorted(positions))
        self.assertIn("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a", workflow)
        self.assertIn("actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c", workflow)


class ReleaseInventoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("release_artifacts", ROOT / "scripts/release_artifacts.py")
        assert spec is not None and spec.loader is not None
        cls.release = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.release)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.tag = "v0.2.1-rc.1"
        self.dist = self.root / "dist"
        self.dist.mkdir()

    @staticmethod
    def header(target):
        # Minimal format fixtures, deliberately not executable programs.
        header = bytearray(32)
        arm = target.startswith("aarch64-")
        if target.endswith("-linux-gnu"):
            header[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<HH", header, 16, 3, 183 if arm else 62)
        else:
            header[:4] = b"\xcf\xfa\xed\xfe"
            struct.pack_into("<III", header, 4, 0x0100000C if arm else 0x01000007, 0, 2)
        return bytes(header)

    def inventory(self, signed=False):
        names = self.release.archive_names(self.tag)
        for name in names:
            (self.dist / name).write_bytes(b"archive test fixture")
        self.release.check(self.tag, "archives", self.dist)
        names.add(f"ardur-agent-{self.tag}.spdx.json")
        (self.dist / f"ardur-agent-{self.tag}.spdx.json").write_text('{"spdxVersion":"SPDX-2.3"}')
        self.release.check(self.tag, "unsigned", self.dist)
        if signed:
            rows = [f"{'0' * 64}  {name}\n" for name in sorted(names)]
            (self.dist / "SHA256SUMS").write_text("".join(rows))
            for name in names | {"SHA256SUMS"}:
                (self.dist / f"{name}.bundle").write_text("bundle inventory fixture; not a signature")
        return names

    def test_packaging_all_targets_preserves_thin_binary_and_executable_mode(self):
        self.assertEqual(set(self.release.TARGETS), set(TARGET_RUNNERS))
        self.assertEqual(set(self.release.BINARIES), {
            "ardur", "ardur-server", "ardur-admin", "ardur-eval",
            "ardur-healthcheck", "ardur-memory-eval",
        })
        for target in self.release.TARGETS:
            with self.subTest(target=target):
                lane = self.root / target
                source = lane / "target" / target / "release"
                source.mkdir(parents=True)
                for name in self.release.BINARIES:
                    binary = source / name
                    binary.write_bytes(self.header(target))
                    binary.chmod(0o755)
                notices = lane / "notices"
                notices.mkdir()
                for notice in ("LICENSE", "ThirdPartyNotices.txt"):
                    (notices / notice).write_text("license test fixture")
                self.release.package(self.tag, target, lane, notices)
                archives = list((lane / "dist").iterdir())
                self.assertEqual(len(archives), 6)
                for archive in archives:
                    name = archive.name.removesuffix(f"-{self.tag}-{target}.tar.gz")
                    with tarfile.open(archive) as tar:
                        expected = [name]
                        if target == "x86_64-apple-darwin":
                            expected += ["licenses/onnxruntime/LICENSE", "licenses/onnxruntime/ThirdPartyNotices.txt"]
                        self.assertEqual(tar.getnames(), expected)
                        member = tar.getmember(name)
                        self.assertTrue(member.isfile())
                        self.assertEqual(member.mode & 0o777, 0o755)
                        stream = tar.extractfile(member)
                        self.assertIsNotNone(stream)
                        assert stream is not None
                        self.assertEqual(stream.read(), self.header(target))

    def test_packaging_refuses_missing_binary_before_creating_archives(self):
        target = self.release.TARGETS[0]
        with self.assertRaisesRegex(ValueError, "missing regular executable"):
            self.release.package(self.tag, target, self.root)
        self.assertEqual(list(self.dist.iterdir()), [])

    def test_intel_packaging_refuses_missing_license_notices(self):
        with self.assertRaisesRegex(ValueError, "requires ONNX Runtime license notices"):
            self.release.package(self.tag, "x86_64-apple-darwin", self.root)
        with self.assertRaisesRegex(ValueError, "missing regular ONNX Runtime notice"):
            self.release.package(self.tag, "x86_64-apple-darwin", self.root, self.root / "absent")

    def test_format_guard_rejects_every_wrong_architecture_and_universal_binary(self):
        for target in self.release.TARGETS:
            for wrong in self.release.TARGETS:
                if wrong != target:
                    with self.subTest(target=target, wrong=wrong):
                        with self.assertRaisesRegex(ValueError, "format/architecture"):
                            self.release.require_binary(self.header(wrong), target)
            with self.assertRaisesRegex(ValueError, "format/architecture"):
                self.release.require_binary(b"\xca\xfe\xba\xbe" + bytes(28), target)

    def test_every_missing_lane_archive_or_bundle_blocks_publication(self):
        self.inventory(signed=True)
        self.release.check(self.tag, "signed", self.dist)
        files = list(self.dist.iterdir())
        self.assertEqual(len(files), 52)
        for path in files:
            data = path.read_bytes()
            path.unlink()
            try:
                with self.subTest(missing=path.name):
                    with self.assertRaisesRegex(ValueError, "inventory mismatch"):
                        self.release.check(self.tag, "signed", self.dist)
            finally:
                path.write_bytes(data)

    def test_extra_empty_and_symlink_assets_are_refused(self):
        self.inventory(signed=True)
        extra = self.dist / "unadvertised.tar.gz"
        extra.write_bytes(b"unexpected fixture")
        with self.assertRaisesRegex(ValueError, "unexpected="):
            self.release.check(self.tag, "signed", self.dist)
        extra.unlink()
        bundle = self.dist / "SHA256SUMS.bundle"
        bundle.write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "empty or non-regular"):
            self.release.check(self.tag, "signed", self.dist)
        bundle.unlink()
        bundle.symlink_to(self.dist / "SHA256SUMS")
        with self.assertRaisesRegex(ValueError, "empty or non-regular"):
            self.release.check(self.tag, "signed", self.dist)

    def test_manifest_must_cover_all_payloads_exactly_once(self):
        self.inventory(signed=True)
        manifest = self.dist / "SHA256SUMS"
        rows = manifest.read_text().splitlines(keepends=True)
        for broken in (rows[:-1], rows + rows[:1], rows[:-1] + rows[:1]):
            manifest.write_text("".join(broken))
            with self.assertRaisesRegex(ValueError, "every archive and the release SBOM exactly once"):
                self.release.check(self.tag, "signed", self.dist)

    def test_workflow_checksum_command_hashes_all_targets_and_detects_tampering(self):
        names = self.inventory()
        workflow = WORKFLOW.read_text()
        block = workflow.split("      - name: Generate SHA256SUMS\n", 1)[1]
        block = block.split("\n      - name:", 1)[0].split("        run: |\n", 1)[1]
        script = "\n".join(line.removeprefix("          ") for line in block.splitlines())
        # GitHub's stock macOS image may expose only shasum, not GNU sha256sum.
        portable = 'command -v sha256sum >/dev/null || sha256sum() { shasum -a 256 "$@"; };\n'
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", portable + script],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = (self.dist / "SHA256SUMS").read_text().splitlines()
        self.assertEqual(len(rows), 25)
        self.assertEqual({row.split("  ", 1)[1] for row in rows}, names)
        for row in rows:
            digest, name = row.split("  ", 1)
            self.assertEqual(digest, hashlib.sha256((self.dist / name).read_bytes()).hexdigest())
        verify = ["bash", "-euo", "pipefail", "-c", portable + "sha256sum --check SHA256SUMS"]
        good = subprocess.run(verify, cwd=self.dist, capture_output=True, text=True)
        self.assertEqual(good.returncode, 0, good.stderr)
        victim = sorted(self.release.archive_names(self.tag))[0]
        (self.dist / victim).write_bytes(b"tampered fixture")
        bad = subprocess.run(verify, cwd=self.dist, capture_output=True, text=True)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn(f"{victim}: FAILED", bad.stdout)

    def test_tag_is_validated_without_normalizing_unsafe_names(self):
        for tag in ("../v1.0.0", "v1.0.0/other", "v1.0.0\n", "$(id)", "v1.0.0 --help"):
            with self.subTest(tag=tag):
                with self.assertRaisesRegex(ValueError, "unsupported release tag"):
                    self.release.archive_names(tag)


if __name__ == "__main__":
    unittest.main()
