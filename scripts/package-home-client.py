#!/usr/bin/env python3
"""Archive exactly the shipping binary for the release matrix's target."""
import os
from pathlib import Path
import tarfile
import zipfile

TARGETS = {'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc'}


def package(target, tag, directory=Path('.')):
    if target not in TARGETS:
        raise ValueError('Unsupported release target')
    if not tag or any(c in tag for c in '/\\\x00'):
        raise ValueError('Release tag cannot be used as an archive name')
    windows = target.endswith('windows-msvc')
    binary_name = 'ardur-rs.exe' if windows else 'ardur-rs'
    binary = directory / 'target' / target / 'release' / binary_name
    if not binary.is_file() or binary.stat().st_size == 0:
        raise ValueError('Shipping binary is missing or empty')
    output = directory / 'dist'
    output.mkdir(exist_ok=True)
    archive = output / f'ardur-rs-{tag}-{target}.{ "zip" if windows else "tar.gz" }'
    if windows:
        with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
            bundle.write(binary, binary_name)
    else:
        with tarfile.open(archive, 'w:gz') as bundle:
            bundle.add(binary, arcname=binary_name)
    return archive


if __name__ == '__main__':
    print(package(os.environ['RELEASE_TARGET'], os.environ['RELEASE_TAG']))
