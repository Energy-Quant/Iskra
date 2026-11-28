#!/usr/bin/env python3
"""Fail-closed delivery from a successful exact-commit CI run; never sign."""
import argparse
import base64
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
REPO = 'Energy-Quant/Iskra'


def gh(*args):
    return subprocess.check_output(['gh', *args], text=True).strip()


def api(path):
    return json.loads(gh('api', path))


def api_all(path, key=None):
    """Every page of a list endpoint. `key` unwraps envelope pages like
    {'workflow_runs': [...]}; bare pages are already item lists."""
    pages = json.loads(gh('api', '--paginate', '--slurp', path))
    return [item for page in pages for item in (page[key] if key else page)]


def source_run(run, channel):
    expected = 'dev' if channel == 'nightly' else 'main'
    if (run.get('conclusion') != 'success' or run.get('head_branch') != expected
            or run.get('head_repository', {}).get('full_name') != REPO
            or run.get('event') not in ('push', 'workflow_dispatch')
            or run.get('path') != '.github/workflows/ci.yml'):
        raise ValueError('release source must be the successful upstream branch CI, never a PR/fork run')
    return expected


def unzip(source, dest):
    with zipfile.ZipFile(source) as archive:
        entries = archive.infolist()
        if len(entries) > 2048 or len(entries) != len({entry.filename for entry in entries}):
            raise ValueError('too many or duplicate artifact members')
        total = 0
        for item in entries:
            path = Path(item.filename)
            if path.is_absolute() or not path.parts or ':' in path.parts[0] or '..' in path.parts or '\\' in item.filename or item.external_attr >> 16 & 0o170000 == 0o120000:
                raise ValueError('unsafe artifact ZIP')
            total += item.file_size
            if total > 128 * 1024 * 1024:
                raise ValueError('artifact too large')
        archive.extractall(dest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('prepare', 'publish', 'nightly', 'auto'))
    parser.add_argument('--run', type=int)
    args = parser.parse_args()
    if args.operation == 'auto':
        sha = api(f'repos/{REPO}/git/ref/heads/main')['object']['sha']
        blob = api(f'repos/{REPO}/contents/plugin/iskra/zeron-plugin.json?ref={sha}')
        version = json.loads(base64.b64decode(blob['content']))['version']
        candidate = next((r for r in api_all(f'repos/{REPO}/releases?per_page=100')
                          if r['tag_name'] == 'v' + version and r['draft']), None)
        if not candidate or len([a for a in candidate['assets'] if a['name'].endswith('.zeron-plugin')]) != 1:
            print('No draft with an author export is ready; nothing is published.')
            return
        runs = api_all(f'repos/{REPO}/actions/workflows/ci.yml/runs?branch=main&per_page=50', 'workflow_runs')
        ready = next((r for r in runs if r['head_sha'] == sha and r['event'] in ('push', 'workflow_dispatch')), None)
        if ready is None or ready['conclusion'] != 'success':
            print('Current main has no latest successful CI; publishing remains closed.')
            return
        args.operation, args.run = 'publish', ready['id']
    if args.run is None:
        parser.error('--run is required except for scheduled auto delivery')
    run = api(f'repos/{REPO}/actions/runs/{args.run}')
    branch = source_run(run, args.operation)
    current = api(f'repos/{REPO}/git/ref/heads/{branch}')['object']['sha']
    if current != run['head_sha']:
        raise ValueError('branch advanced; run CI again on the current commit')
    if args.operation == 'nightly':
        main_sha = api(f'repos/{REPO}/git/ref/heads/main')['object']['sha']
        if current == main_sha:
            print('Current dev matches promoted main; skipping duplicate nightly.')
            return
    recent = api_all(f'repos/{REPO}/actions/workflows/ci.yml/runs?branch={branch}&per_page=100', 'workflow_runs')
    latest = next((r for r in recent if r['head_sha'] == current and r['event'] in ('push', 'workflow_dispatch')), None)
    if latest is None or latest['id'] != args.run or latest['conclusion'] != 'success':
        raise ValueError('only the latest exact-SHA source CI is eligible; an older green run is insufficient')

    attempt = run.get('run_attempt', 1)
    jobs = api_all(f'repos/{REPO}/actions/runs/{args.run}/attempts/{attempt}/jobs?per_page=100', 'jobs')
    if not any(j['name'] == 'CI Gate' and j['conclusion'] == 'success' and j['head_sha'] == run['head_sha'] for j in jobs):
        raise ValueError('the required aggregate CI Gate did not succeed')
    with tempfile.TemporaryDirectory(prefix='iskra-delivery-') as temp:
        root = Path(temp)
        gh('run', 'download', str(args.run), '--repo', REPO, '--dir', str(root / 'artifacts'))
        unsigned = next((root / 'artifacts').rglob('iskra-*/zeron-plugin.json')).parent
        reference = {p.relative_to(unsigned).as_posix(): p.read_bytes() for p in unsigned.rglob('*') if p.is_file()}
        archives = list((root / 'artifacts').rglob('iskra-plugin-UNSIGNED-*.zip'))
        if {p.name for p in archives} != {f'iskra-plugin-UNSIGNED-{p}.zip' for p in ('linux', 'windows', 'macos')}:
            raise ValueError('all three platform packages are required')
        for index, archive in enumerate(archives):
            extracted = root / f'portable-{index}'
            unzip(archive, extracted)
            actual = {p.relative_to(extracted).as_posix(): p.read_bytes() for p in extracted.rglob('*') if p.is_file()}
            if actual != reference:
                raise ValueError('portable plugin payloads differ across OS runners')
        manifest = json.loads((unsigned / 'zeron-plugin.json').read_text())
        version = manifest['version']
        if not re.fullmatch(r'\d+\.\d+\.\d+', version):
            raise ValueError('stable version must be X.Y.Z')
        tag = f'nightly-{run["head_sha"][:12]}-{args.run}' if args.operation == 'nightly' else 'v' + version
        releases = api_all(f'repos/{REPO}/releases?per_page=100')
        release = next((r for r in releases if r['tag_name'] == tag), None)
        if release and not release['draft']:
            if args.operation == 'publish':
                raise ValueError('published releases are immutable to this workflow')
            print('This version already exists; prepare a version-bump PR for a new stable release.')
            return
        if release and release['target_commitish'] != run['head_sha']:
            raise ValueError('candidate targets another commit; choose a new version')
        delivery = root / 'delivery'; delivery.mkdir()
        for path in (root / 'artifacts').rglob('*'):
            if path.is_file() and (path.name.endswith('.tar.gz') or path.name.startswith('iskra-avatar-tools-') or path.name == 'zeron-pet-bar-linux-x86_64'):
                shutil.copy2(path, delivery / path.name)
            if args.operation == 'nightly' and path.name.startswith('iskra-plugin-UNSIGNED-'):
                shutil.copy2(path, delivery / path.name)
        if args.operation == 'prepare':
            spec = importlib.util.spec_from_file_location('portable', ROOT / 'scripts/package-portable.py')
            portable = importlib.util.module_from_spec(spec); spec.loader.exec_module(portable)
            portable.package(unsigned, delivery / 'iskra-staging-UNSIGNED.zip')
        if args.operation == 'publish':
            if not release:
                raise ValueError('prepare a draft before attaching an author export')
            exports = [a for a in release['assets'] if a['name'].endswith('.zeron-plugin')]
            if len(exports) != 1:
                raise ValueError('attach exactly one author-exported .zeron-plugin to the draft')
            gh('release', 'download', tag, '--repo', REPO, '--pattern', exports[0]['name'], '--dir', str(delivery))
            package = delivery / exports[0]['name']
            spec = importlib.util.spec_from_file_location('verify', ROOT / 'scripts/verify-signed-plugin.py')
            verify = importlib.util.module_from_spec(spec); spec.loader.exec_module(verify)
            verify.verify(package, unsigned, json.loads((ROOT / '.github/trusted-signers.json').read_text()))
            spec = importlib.util.spec_from_file_location('portable', ROOT / 'scripts/package-portable.py')
            portable = importlib.util.module_from_spec(spec); spec.loader.exec_module(portable)
            for platform in ('linux', 'windows', 'macos'):
                wrapper = root / ('wrapper-' + platform); wrapper.mkdir()
                shutil.copyfile(package, wrapper / package.name)
                (wrapper / 'INSTALL.txt').write_text('Install the .zeron-plugin in Zeron Settings > Plugins. Review permissions. Desktop companion is Linux-only.\n', encoding='utf-8', newline='\n')
                portable.package(wrapper, delivery / f'iskra-plugin-{platform}.zip')
        import hashlib
        (delivery / 'SHA256SUMS').write_text(''.join(hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + p.name + '\n' for p in sorted(delivery.iterdir()) if p.is_file()))
        notes = root / 'NOTES.md'
        notes.write_text(f'Source: {run["head_sha"]}\nCI: {run["html_url"]}\n\n' +
                         ('UNSIGNED developer preview. Not an installable signed Zeron plugin.\n' if args.operation == 'nightly' else
                          'Plugin and avatar tools: Windows, macOS, Linux. Native desktop: Linux/Wayland x86_64 only.\nAuthor signing remains local; CI verifies the Ed25519 signature and exact reviewed payload.\n'))
        release_notes = ROOT / 'docs/releases' / (version + '.md')
        if args.operation != 'nightly' and release_notes.is_file():
            notes.write_text(release_notes.read_text(encoding='utf-8') + '\n\n' + notes.read_text(encoding='utf-8'), encoding='utf-8')
        if not release:
            command = ['release', 'create', tag, '--repo', REPO, '--target', run['head_sha'], '--title', tag, '--notes-file', str(notes)]
            command += ['--draft']
            if args.operation == 'nightly':
                command += ['--prerelease']
            gh(*command)
        gh('release', 'upload', tag, '--repo', REPO, '--clobber', *[str(p) for p in sorted(delivery.iterdir())])
        if args.operation == 'nightly':
            gh('release', 'edit', tag, '--repo', REPO, '--draft=false', '--prerelease', '--latest=false', '--notes-file', str(notes))
        if args.operation == 'publish':
            for a in release['assets']:
                if 'UNSIGNED' in a['name']:
                    gh('release', 'delete-asset', tag, a['name'], '--repo', REPO, '--yes')
            gh('release', 'edit', tag, '--repo', REPO, '--draft=false', '--latest', '--notes-file', str(notes))
        print(tag, args.operation, 'complete')


if __name__ == '__main__':
    main()
