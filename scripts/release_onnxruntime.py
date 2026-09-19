#!/usr/bin/env python3
"""Pinned static ONNX Runtime input for the Intel macOS release lane only."""
import argparse
import hashlib
import io
import json
import os
import pathlib
import urllib.request
import zipfile

VERSION = "1.28.0"
ARCHIVE_ROOT = f"onnxruntime-osx-x86_64-static_lib-{VERSION}"
ARCHIVE_URL = (
    "https://github.com/csukuangfj/onnxruntime-libs/releases/download/"
    f"v{VERSION}/{ARCHIVE_ROOT}.zip"
)
ARCHIVE_SHA256 = "88c6037c0eb9a7f0013729e0181feec4fb9ef30ca37769aa7b69d2478450c415"
SOURCE_COMMIT = "da9b5e364c465de65c49d91e696cd6485270757f"
NOTICES = {
    "LICENSE": "2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c",
    "ThirdPartyNotices.txt": "0e07b95f3a8d6230037707c5c4a2b554d12c4cb67369669ac255635528ffcee2",
}


def download(url, expected):
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read(64 * 1024 * 1024 + 1)
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        raise ValueError(f"SHA256 mismatch for {url}: expected {expected}, got {actual}")
    print(f"SHA256 OK: {expected} {url}", flush=True)
    return data


def prepare(destination):
    archive = download(ARCHIVE_URL, ARCHIVE_SHA256)
    with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
        # Extract a single known member, never arbitrary upstream paths/symlinks.
        library = zipped.read(f"{ARCHIVE_ROOT}/lib/libonnxruntime.a")
    if not library.startswith(b"!<arch>\n"):
        raise ValueError("expected a static ar library")
    notices = {
        name: download(f"https://raw.githubusercontent.com/microsoft/onnxruntime/{SOURCE_COMMIT}/{name}", digest)
        for name, digest in NOTICES.items()
    }
    (destination / "lib").mkdir(parents=True, exist_ok=True)
    (destination / "notices").mkdir(exist_ok=True)
    (destination / "lib/libonnxruntime.a").write_bytes(library)
    for name, data in notices.items():
        (destination / "notices" / name).write_bytes(data)


def annotate_sbom(path):
    document = json.loads(path.read_text())
    identifier = "SPDXRef-onnxruntime-intel-macos-static"
    if not document.get("spdxVersion", "").startswith("SPDX-2."):
        raise ValueError("expected SPDX 2 release SBOM")
    packages = document["packages"]
    if any(package["SPDXID"] == identifier for package in packages):
        raise ValueError("release SBOM already contains the Intel native dependency")
    packages.append({
        "SPDXID": identifier,
        "name": "onnxruntime-intel-macos-static",
        "versionInfo": VERSION,
        "downloadLocation": ARCHIVE_URL,
        "filesAnalyzed": False,
        "checksums": [{"algorithm": "SHA256", "checksumValue": ARCHIVE_SHA256}],
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "MIT",
        "copyrightText": "NOASSERTION",
        "sourceInfo": (
            "Static x86_64-apple-darwin build supplied by csukuangfj/onnxruntime-libs; "
            f"Microsoft ONNX Runtime {VERSION}. Input archive SHA256 is pinned; "
            "this is not an upstream build-provenance attestation. "
            f"License/third-party notices from Microsoft source commit {SOURCE_COMMIT}."
        ),
    })
    document.setdefault("relationships", []).append({
        "spdxElementId": document["SPDXID"],
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": identifier,
    })
    path.write_text(json.dumps(document, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_command = commands.add_parser("prepare")
    prepare_command.add_argument("--directory", type=pathlib.Path, required=True)
    sbom = commands.add_parser("sbom")
    sbom.add_argument("path", type=pathlib.Path)
    args = parser.parse_args()
    if args.command == "prepare":
        directory = args.directory.resolve()
        prepare(directory)
        with open(os.environ["GITHUB_ENV"], "a") as env:
            env.write(f"ORT_LIB_PATH={directory / 'lib'}\n")
            env.write("ORT_PREFER_DYNAMIC_LINK=0\n")
            env.write(f"ORT_RELEASE_NOTICES={directory / 'notices'}\n")
    else:
        annotate_sbom(args.path)


if __name__ == "__main__":
    main()
