"""Check all archive formats and ensure legacy binaries cannot enter them."""
import importlib.util
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location('packaging', Path(__file__).with_name('package-home-client.py'))
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


class PackageTests(unittest.TestCase):
    def test_each_target_contains_only_the_client(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for target in sorted(packaging.TARGETS):
                windows = target.endswith('windows-msvc')
                name = 'ardur-rs.exe' if windows else 'ardur-rs'
                release = root / 'target' / target / 'release'
                release.mkdir(parents=True)
                (release / name).write_bytes(b'client')
                (release / 'legacy-server').write_bytes(b'excluded')
                archive = packaging.package(target, 'v1.0.0', root)
                if windows:
                    with zipfile.ZipFile(archive) as bundle:
                        self.assertEqual(bundle.namelist(), [name])
                        self.assertEqual(bundle.read(name), b'client')
                else:
                    with tarfile.open(archive) as bundle:
                        self.assertEqual(bundle.getnames(), [name])
                        self.assertEqual(bundle.extractfile(name).read(), b'client')

    def test_missing_client_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                packaging.package('x86_64-unknown-linux-gnu', 'v1.0.0', Path(directory))


if __name__ == '__main__':
    unittest.main()
