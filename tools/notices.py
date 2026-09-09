#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate distribution notices from the locked Linux dependency graphs, offline."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / 'packaging/THIRD-PARTY-NOTICES.txt'
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')
BINARIES = ('skerry', 'skerry-archive', 'skerry-thumbnailer', 'skerry-preview-doc')


def packages():
    selected = {}
    for target in TARGETS:
        metadata = json.loads(subprocess.check_output([
            'cargo', 'metadata', '--locked', '--offline', '--format-version', '1',
            '--filter-platform', target], cwd=ROOT))
        by_id = {package['id']: package for package in metadata['packages']}
        nodes = {node['id']: node for node in metadata['resolve']['nodes']}
        pending = [key for key, package in by_id.items() if package['name'] in BINARIES]
        visited = set()
        while pending:
            key = pending.pop()
            if key in visited:
                continue
            visited.add(key)
            package = by_id[key]
            if package['source']:
                selected[key] = package
            for edge in nodes[key]['deps']:
                if any(kind['kind'] != 'dev' for kind in edge['dep_kinds']):
                    pending.append(edge['pkg'])
    return sorted(selected.values(), key=lambda package: (package['name'], package['version']))


def render():
    lock = hashlib.sha256((ROOT / 'Cargo.lock').read_bytes()).hexdigest()
    sections = [f'''Skerry third-party notices
Cargo.lock SHA-256: {lock}

Includes the union of dependencies for both Linux binary architectures, including
build dependencies as a conservative superset. Runtime system libraries and fonts
are not bundled. The Rust compiler's standard library notices are distributed
separately with the release toolchain notices. Skerry's LICENSE and NOTICE apply
to Skerry's own source. Individual dependency terms follow.

Apache License 2.0 (also applies where a dependency offers it as an alternative):

{(ROOT / 'LICENSE').read_text()}
''']
    count = 0
    for package in packages():
        root = Path(package['manifest_path']).parent
        files = sorted(path for path in root.rglob('*') if path.is_file() and
                       path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'COPYRIGHT', 'NOTICE')))
        supplement = ROOT / 'packaging/upstream-licenses' / f"{package['name']}-{package['version']}.txt"
        if not files and not supplement.is_file():
            raise ValueError(f"missing license text: {package['name']} {package['version']}")
        sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\nSPDX: {package['license']}\nRepository: {package['repository']}\nAuthors: {', '.join(package['authors'])}\n")
        for path in files:
            sections.append(f'\n--- {path.relative_to(root)} ---\n' + path.read_text(errors='strict'))
        if supplement.is_file():
            sections.append('\n--- Upstream supplement ---\n' + supplement.read_text())
        # Preserve copyright statements which upstream placed only in a README.
        for path in sorted(root.glob('README*')):
            if path.is_file() and 'copyright' in path.read_text(errors='replace').lower():
                sections.append('\n--- ' + path.name + ' ---\n' + path.read_text())
        count += 1
    cc0 = ROOT / 'packaging/upstream-licenses/CC0-1.0.txt'
    if any(package['license'] == 'CC0-1.0' for package in packages()):
        sections.append('\n--- CC0 1.0 license ---\n' + cc0.read_text())
    return '\n'.join(line.rstrip() for line in '\n'.join(sections).splitlines()).rstrip() + '\n', count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    text, count = render()
    if args.check:
        if not OUTPUT.is_file() or OUTPUT.read_text() != text:
            parser.exit(1, 'Dependency notices are stale; run python3 tools/notices.py\n')
    else:
        OUTPUT.write_text(text)
    print(f'dependency notices: {count} packages verified')


if __name__ == '__main__':
    main()
