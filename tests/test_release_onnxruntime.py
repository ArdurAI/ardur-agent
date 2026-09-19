"""Intel macOS native dependency guards; no network in unit tests."""
import importlib.util
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]


class IntelReleaseWorkflowTests(unittest.TestCase):
    def test_intel_workflow_requires_verified_static_dependency(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("if: matrix.target == 'x86_64-apple-darwin'", workflow)
        self.assertIn("python3 scripts/release_onnxruntime.py prepare", workflow)
        self.assertIn('lipo -archs "${ORT_LIB_PATH}/libonnxruntime.a"', workflow)
        self.assertLess(workflow.index("scripts/release_onnxruntime.py prepare"),
                        workflow.index("cargo build --workspace"))
        self.assertIn("python3 scripts/release_onnxruntime.py sbom", workflow)
        self.assertLess(workflow.index("scripts/release_onnxruntime.py sbom"),
                        workflow.index("name: Generate SHA256SUMS"))


class IntelReleaseDependencyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("release_onnxruntime", ROOT / "scripts/release_onnxruntime.py")
        assert spec is not None and spec.loader is not None
        cls.ort = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.ort)

    def test_supplier_version_and_downloads_are_exactly_pinned(self):
        self.assertEqual(self.ort.VERSION, "1.28.0")
        self.assertEqual(self.ort.ARCHIVE_SHA256, "88c6037c0eb9a7f0013729e0181feec4fb9ef30ca37769aa7b69d2478450c415")
        self.assertIn("csukuangfj/onnxruntime-libs/releases/download/v1.28.0/", self.ort.ARCHIVE_URL)
        self.assertEqual(self.ort.SOURCE_COMMIT, "da9b5e364c465de65c49d91e696cd6485270757f")
        self.assertEqual(set(self.ort.NOTICES), {"LICENSE", "ThirdPartyNotices.txt"})
        for digest in self.ort.NOTICES.values():
            self.assertRegex(digest, r"^[a-f0-9]{64}$")

    def test_download_checksum_is_required_before_extraction(self):
        with mock.patch.object(self.ort.urllib.request, "urlopen", return_value=io.BytesIO(b"tampered archive")):
            with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                self.ort.download(self.ort.ARCHIVE_URL, self.ort.ARCHIVE_SHA256)

    def test_prepare_extracts_only_named_static_library_and_requires_notices(self):
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as zipped:
            zipped.writestr(f"{self.ort.ARCHIVE_ROOT}/lib/libonnxruntime.a", b"!<arch>\nfixture")
            zipped.writestr("../../outside", b"must never extract")
        with tempfile.TemporaryDirectory() as temporary:
            dest = pathlib.Path(temporary) / "ort"
            with mock.patch.object(self.ort, "download", side_effect=[archive.getvalue(), b"MIT fixture", b"notice fixture"]):
                self.ort.prepare(dest)
            self.assertEqual((dest / "lib/libonnxruntime.a").read_bytes(), b"!<arch>\nfixture")
            self.assertEqual({x.name for x in (dest / "notices").iterdir()}, set(self.ort.NOTICES))
            self.assertFalse((pathlib.Path(temporary) / "outside").exists())

    def test_release_sbom_records_the_additional_native_supplier_and_digest(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "release.spdx.json"
            original = {"spdxVersion": "SPDX-2.3", "SPDXID": "SPDXRef-DOCUMENT",
                        "packages": [{"SPDXID": "SPDXRef-cargo-package", "name": "existing"}],
                        "relationships": []}
            path.write_text(json.dumps(original))
            self.ort.annotate_sbom(path)
            sbom = json.loads(path.read_text())
            self.assertEqual(sbom["packages"][0], original["packages"][0])
            self.assertEqual(len(sbom["packages"]), 2)
            native = sbom["packages"][1]
            self.assertEqual(native["versionInfo"], self.ort.VERSION)
            self.assertEqual(native["downloadLocation"], self.ort.ARCHIVE_URL)
            self.assertEqual(native["checksums"], [{"algorithm": "SHA256", "checksumValue": self.ort.ARCHIVE_SHA256}])
            self.assertIn("csukuangfj/onnxruntime-libs", native["sourceInfo"])
            self.assertIn("x86_64-apple-darwin", native["sourceInfo"])
            self.assertEqual(sbom["relationships"][-1]["relatedSpdxElement"], native["SPDXID"])
            with self.assertRaisesRegex(ValueError, "already contains"):
                self.ort.annotate_sbom(path)


if __name__ == "__main__":
    unittest.main()
