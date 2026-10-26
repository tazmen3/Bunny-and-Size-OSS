#!/usr/bin/env python3
"""Verify a local v0.3 candidate's manifest, identity and absence of private inputs.

Read-only; never publishes. Accepts binary or corresponding-source tar.gz / ZIP.
"""
import argparse
import hashlib
import json
import stat
import re
from pathlib import Path, PurePosixPath
import tarfile
import zipfile


# Reserved original; a manifest or filename change cannot authorize its pixels.
PRIVATE_ARTWORK_SHA256 = '683c568876149bb0b2e99114e4b6e640ff46445788e3a5e26e50142ccb6e6bea'


def reject_private_artwork(name, raw):
    if 'tazmenworld-original' in name.lower() or 'portrait' in name.lower():
        raise ValueError('Private artwork member: ' + name)
    if hashlib.sha256(raw).hexdigest() == PRIVATE_ARTWORK_SHA256:
        raise ValueError('Private artwork content: ' + name)
    # PE/ELF may embed a PNG under a completely different resource name.
    start = raw.find(b'\x89PNG\r\n\x1a\n')
    while start >= 0:
        end = raw.find(b'IEND\xaeB`\x82', start)
        if end >= 0 and hashlib.sha256(raw[start:end + 8]).hexdigest() == PRIVATE_ARTWORK_SHA256:
            raise ValueError('Private artwork embedded in: ' + name)
        start = raw.find(b'\x89PNG\r\n\x1a\n', start + 8)


def verify(path, branding=False, internal_review=False):
    if path.suffix == '.zip':
        with zipfile.ZipFile(path) as archive:
            entries = []
            for member in archive.infolist():
                mode = member.external_attr >> 16
                if stat.S_ISLNK(mode) or (stat.S_IFMT(mode) and not (stat.S_ISREG(mode) or stat.S_ISDIR(mode))):
                    raise ValueError('Unexpected non-file member: ' + member.filename)
                if not member.is_dir():
                    entries.append((member.filename, archive.read(member)))
    else:
        with tarfile.open(path) as archive:
            entries = []
            for member in archive:
                if not member.isfile():
                    raise ValueError('Unexpected non-file member: ' + member.name)
                entries.append((member.name, archive.extractfile(member).read()))
    if len({name for name, _ in entries}) != len(entries):
        raise ValueError('Duplicate archive entries')
    roots = {name.split('/')[0] for name, _ in entries}
    if len(roots) != 1 or not next(iter(roots)).startswith('Bunny-and-Size-v0.3.'):
        raise ValueError('Unexpected archive root')
    data = {}
    for name, raw in entries:
        safe = PurePosixPath(name)
        if safe.is_absolute() or '..' in safe.parts or '\\' in name:
            raise ValueError('Unsafe archive member: ' + name)
        relative = name.split('/', 1)[1]
        if any(part in ('.git', 'private', 'target', '__pycache__') for part in safe.parts) or relative == 'AGENTS.md' or relative.startswith('docs/validation/'):
            raise ValueError('Private/build member: ' + relative)
        if not relative.startswith('vendor-registry/'):
            patterns = (rb'/(?:media|home)/[A-Za-z0-9][A-Za-z0-9_.-]*/',
                        rb'C:[/\\]SortSizeLab', rb'(?:github_pat|ghp)_[A-Za-z0-9]{10,}')
            if any(re.search(pattern, raw) for pattern in patterns):
                raise ValueError('Private path or token marker in: ' + relative)
        reject_private_artwork(relative, raw)
        data[relative] = raw
    sha = lambda raw: hashlib.sha256(raw).hexdigest()
    manifest = json.loads(data['FILES.json'])
    if {row['path'] for row in manifest} != set(data) - {'FILES.json', 'SHA256SUMS'}:
        raise ValueError('Manifest membership mismatch')
    for row in manifest:
        if sha(data[row['path']]) != row['sha256'] or len(data[row['path']]) != row['bytes']:
            raise ValueError('Manifest hash/size mismatch: ' + row['path'])
    checksum_lines = data['SHA256SUMS'].decode().splitlines()
    checksum_names = [line.split('  ', 1)[1] for line in checksum_lines]
    if len(checksum_names) != len(set(checksum_names)) or set(checksum_names) != set(data) - {'SHA256SUMS'}:
        raise ValueError('SHA256SUMS membership mismatch')
    for line in checksum_lines:
        digest, name = line.split('  ', 1)
        if sha(data[name]) != digest:
            raise ValueError('SHA256SUMS mismatch: ' + name)
    branding = branding or 'BRANDING.md' in data or any(n.startswith('assets/branding/') for n in data)
    info = json.loads(data['BUILDINFO.json'])
    if not info['version'].startswith('0.3.'):
        raise ValueError('Unexpected product version')
    if branding:
        rights = info.get('artwork_rights', {})
        if not internal_review and (rights.get('status') != 'approved' or not info.get('redistribution_ready')):
            raise ValueError('Artwork owner approval missing; internal review only')
    source = '-source-' in path.name
    windows = '-windows-' in path.name
    generated = {'LICENSE', 'docs/CLI.md', 'docs/GUI.md', 'docs/ENTRIES_EXPORT.md', 'FILES.json', 'SHA256SUMS', 'BUILDINFO.json', 'README.md', 'AUTHORS.txt',
                 'THIRD_PARTY_NOTICES.txt', 'USER_GUIDE.md', 'COMPATIBILITY.md', 'RESERVATIONS.md',
                 'MIGRATION_V03.md', 'CREDITS.md', 'BRANDING.md'}
    if source:
        approved = set(json.loads((Path(__file__).resolve().parents[1] / 'packaging/v03-public-files.json').read_text()))
        generated |= {'SOURCE_COMPONENTS.json', 'SOURCE_LEGAL_SUPPLEMENTS.json', '.cargo/config.toml',
                      'rebuild-linux.sh', 'rebuild-windows.ps1'}
        unknown = set(data) - approved - generated
        unknown = {n for n in unknown if not n.startswith(('licences/', 'vendor-registry/'))}
    else:
        suffix = '.exe' if windows else ''
        generated |= {n + suffix for n in ['bunny-and-size', 'bunny-and-size-cli', 'sortsize']}
        generated.add('COMPONENTS.json')
        if branding and not windows:
            generated.add('share/applications/bunny-and-size.desktop')
            generated |= {f'share/icons/hicolor/{size}x{size}/apps/bunny-and-size.png'
                          for size in (16,24,32,48,64,128,256,512)}
        unknown = {n for n in set(data) - generated if not n.startswith('licences/')}
    if unknown:
        raise ValueError('Unapproved archive files: ' + ', '.join(sorted(unknown)))
    if source:
        required = ['Cargo.lock', 'spikes/slint/Cargo.lock', 'apps/sortsize-cli/src/lib.rs',
                    'vendor/i-slint-backend-winit-1.18.1/LICENSES/GPL-3.0-only.txt', 'vendor/provenance.json']
    else:
        suffix = '.exe' if windows else ''
        required = [name + suffix for name in ['bunny-and-size', 'bunny-and-size-cli', 'sortsize']]
        for name in required:
            if sha(data[name]) != info['binaries'][name]['sha256']:
                raise ValueError('Executable provenance mismatch: ' + name)
    if branding:
        required += ['CREDITS.md', 'BRANDING.md']
        if not internal_review:
            required += ['licences/artwork/CC-BY-SA-4.0.txt', 'licences/artwork/PROVENANCE.json']
            artwork = json.loads(data['licences/artwork/PROVENANCE.json'])
            approval = artwork.get('owner_approval', {})
            if (artwork.get('license') != 'CC-BY-SA-4.0' or artwork.get('license_status') != 'approved'
                    or approval.get('files') != artwork.get('files')
                    or approval.get('owner') != artwork.get('author')
                    or not approval.get('date') or not approval.get('evidence')):
                raise ValueError('Invalid artwork approval')
            expected_license = (Path(__file__).resolve().parents[1] / 'assets/branding/CC-BY-SA-4.0.txt').read_bytes()
            if data['licences/artwork/CC-BY-SA-4.0.txt'] != expected_license:
                raise ValueError('Artwork license text mismatch')
            if source:
                for name, digest in artwork['files'].items():
                    if name not in data or sha(data[name]) != digest:
                        raise ValueError('Artwork source hash mismatch: ' + name)
        if source:
            required += ['assets/branding/mascot-master.png', 'assets/branding/bunny-and-size.ico']
        elif not windows:
            required += ['share/applications/bunny-and-size.desktop', 'share/icons/hicolor/32x32/apps/bunny-and-size.png']
    required += ['LICENSE', 'AUTHORS.txt', 'THIRD_PARTY_NOTICES.txt']
    if any(name not in data for name in required):
        raise ValueError('Missing required file')
    return dict(archive=path.name, sha256=sha(path.read_bytes()), source_commit=info['source_commit'],
                files=len(data), manifest_verified=True, private_patterns_found=False, branding=branding)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archives', nargs='+', type=Path)
    parser.add_argument('--branding', action='store_true')
    parser.add_argument('--internal-review', action='store_true', help='Inspect private test candidates; no publication authorization')
    args = parser.parse_args()
    print(json.dumps([verify(path, args.branding, args.internal_review) for path in args.archives], indent=2))


if __name__ == '__main__':
    main()
