#!/usr/bin/env python3
"""Check a cargo-installed binary without consulting the user's paired profile."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory() as directory:
    env = dict(os.environ, HOME=directory, XDG_CONFIG_HOME=directory, APPDATA=directory)
    for flag in ['--version', '--help']:
        output = subprocess.run([str(binary), flag], env=env, capture_output=True, text=True, check=True)
        assert output.stdout and not output.stderr, output
        if flag == '--version':
            assert 'home contract ' in output.stdout, output.stdout
        print(output.stdout, end='')
    output = subprocess.run([str(binary), 'status', '--json'], env=env, capture_output=True, text=True)
    assert output.returncode == 2, output
    assert not output.stderr, output.stderr
    value = json.loads(output.stdout)
    assert value['ok'] is False and value['exitCode'] == 2, value
    if os.name == 'nt':
        # Windows still refuses private-file storage until protected ACLs exist.
        assert value['error']['code'] == 'unsafe_storage', value
        assert 'Private pairing storage is unavailable or unsafe' in value['error']['message'], value
    else:
        assert value['error']['code'] == 'not_paired', value
        assert value['error']['message'] == 'No paired home profile; pair this device first.', value
    print('Installed client smoke passed; unpaired status exits 2 with a clear diagnosis.')
