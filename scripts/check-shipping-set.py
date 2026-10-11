#!/usr/bin/env python3
"""Fail if the shipping client's normal graph gains execution or service crates.

Only home protocol, transport, CLI, and pure remote evaluation workspace crates
are allowed. This also rejects newly added workspace crates until reviewed.
"""
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALLOWED = {'ardur-rs', 'home-client', 'home-protocol', 'ardur-eval'}
# Direct execution/service surfaces found in the workspace. Prefixes cover all
# providers and channels, including future ones. The workspace allowlist also
# excludes scheduler, local memory, media, integration, and test harness crates.
FORBIDDEN = {
    'runtime', 'fused-runtime', 'server', 'multi-agent', 'delegate-tool', 'cli',
    'messaging-gateway', 'code-execution', 'browser', 'terminal',
    'automation', 'cron', 'standing-goals', 'plugin-runtime', 'tool-registry',
    'slack-adapter', 'web', 'webhook', 'admin', 'cron-ui', 'embeddings',
}


def rejected(names, workspace):
    result = []
    for name in names:
        if name in ALLOWED:
            continue
        short = name.removeprefix('ardur-')
        if name in workspace or short in FORBIDDEN or short.startswith(('provider-', 'channel-')):
            result.append(name)
    return sorted(result)


def cargo(*arguments):
    return subprocess.check_output(['cargo', *arguments], cwd=ROOT, text=True)


def main():
    metadata = json.loads(cargo('metadata', '--offline', '--locked', '--no-deps', '--format-version', '1'))
    members = set(metadata['workspace_members'])
    workspace = {p['name'] for p in metadata['packages'] if p['id'] in members}
    tree = cargo('tree', '-p', 'ardur-rs', '-e', 'normal', '--offline', '--locked',
                 '--prefix', 'none', '--format', '{p}')
    names = {line.split()[0] for line in tree.splitlines() if line.strip()}
    if not ALLOWED <= names:
        sys.exit('Shipping graph is incomplete; inspect cargo tree output.')
    bad = rejected(names, workspace)
    if bad:
        sys.exit('Shipping client contains execution/service crates: ' + ', '.join(bad))
    print(f'Shipping set passed: {len(names)} normal dependencies; no local execution or service crates.')


if __name__ == '__main__':
    main()
