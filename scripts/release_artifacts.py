#!/usr/bin/env python3
"""Package native release binaries and refuse incomplete release inventories.

Crypto verification is deliberately performed by cosign in release.yml, after
this structural check. A nonempty bundle alone is not a valid signature.
"""

import argparse
import os
import pathlib
import re
import struct
import subprocess
import tarfile

BINARIES = (
    "ardur", "ardur-server", "ardur-admin", "ardur-eval",
    "ardur-healthcheck", "ardur-memory-eval",
)
TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
)


def validate_tag(tag):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?", tag):
        raise ValueError(f"unsupported release tag: {tag!r}")


def archive_names(tag):
    validate_tag(tag)
    return {f"{name}-{tag}-{target}.tar.gz" for target in TARGETS for name in BINARIES}


def require_binary(header, target):
    """Check thin Mach-O / ELF64 machine headers, not the filename's claim."""
    if len(header) < 32:
        raise ValueError(f"truncated binary for {target}")
    arm = target.startswith("aarch64-")
    if target.endswith("-linux-gnu"):
        executable, machine = struct.unpack_from("<HH", header, 16)
        valid = (header[:6] == b"\x7fELF\x02\x01" and executable in (2, 3)
                 and machine == (183 if arm else 62))
    else:
        cpu, _, executable = struct.unpack_from("<III", header, 4)
        valid = (header[:4] == b"\xcf\xfa\xed\xfe" and executable == 2
                 and cpu == (0x0100000C if arm else 0x01000007))
    if not valid:
        raise ValueError(f"binary format/architecture does not match {target}")


def package(tag, target, root=pathlib.Path("."), notices=None):
    validate_tag(tag)
    if target not in TARGETS:
        raise ValueError(f"unsupported target: {target}")
    notice_files = []
    if target == "x86_64-apple-darwin":
        if notices is None:
            raise ValueError("Intel macOS packaging requires ONNX Runtime license notices")
        for name in ("LICENSE", "ThirdPartyNotices.txt"):
            notice = pathlib.Path(notices) / name
            if notice.is_symlink() or not notice.is_file() or notice.stat().st_size == 0:
                raise ValueError(f"missing regular ONNX Runtime notice: {notice}")
            notice_files.append(notice)
    source = root / "target" / target / "release"
    # Validate every source before creating even the first archive.
    for name in BINARIES:
        binary = source / name
        if binary.is_symlink() or not binary.is_file() or not binary.stat().st_mode & 0o111:
            raise ValueError(f"missing regular executable: {binary}")
        with binary.open("rb") as stream:
            require_binary(stream.read(32), target)
        subprocess.run(["file", str(binary)], check=True)
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    if any(dist.iterdir()):
        raise ValueError("refusing to package into a nonempty dist directory")
    for name in BINARIES:
        archive = dist / f"{name}-{tag}-{target}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(source / name, arcname=name, recursive=False)
            for notice in notice_files:
                tar.add(notice, arcname=f"licenses/onnxruntime/{notice.name}", recursive=False)
        print(archive, flush=True)


def check(tag, stage, dist=pathlib.Path("dist")):
    expected = archive_names(tag)
    if stage in ("unsigned", "signed"):
        expected.add(f"ardur-agent-{tag}.spdx.json")
    payloads = expected.copy()
    if stage == "signed":
        expected.add("SHA256SUMS")
        expected |= {f"{name}.bundle" for name in expected.copy()}
    actual = {path.name for path in dist.iterdir()}
    if actual != expected:
        raise ValueError(f"release inventory mismatch: missing={sorted(expected - actual)}, "
                         f"unexpected={sorted(actual - expected)}")
    for name in sorted(expected):
        path = dist / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"empty or non-regular release asset: {name}")
    if stage == "signed":
        # sha256sum --check alone accepts an incomplete or duplicate manifest.
        rows = (dist / "SHA256SUMS").read_text().splitlines()
        names = []
        for row in rows:
            match = re.fullmatch(r"[0-9a-f]{64}  (.+)", row)
            if not match:
                raise ValueError(f"malformed checksum row: {row!r}")
            names.append(match[1])
        if set(names) != payloads or len(names) != len(payloads):
            raise ValueError("checksum manifest must cover every archive and the release SBOM exactly once")
    print(f"release inventory OK: {len(expected)} {stage} files", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    pack = commands.add_parser("package")
    pack.add_argument("--tag", required=True)
    pack.add_argument("--target", choices=TARGETS, required=True)
    inventory = commands.add_parser("check")
    inventory.add_argument("--tag", required=True)
    inventory.add_argument("--stage", choices=("archives", "unsigned", "signed"), required=True)
    args = parser.parse_args()
    try:
        if args.command == "package":
            package(args.tag, args.target, notices=os.environ.get("ORT_RELEASE_NOTICES"))
        else:
            check(args.tag, args.stage)
    except (OSError, ValueError) as error:
        parser.exit(1, f"release refused: {error}\n")


if __name__ == "__main__":
    main()
