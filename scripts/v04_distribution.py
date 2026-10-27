#!/usr/bin/env python3
"""Local V0.4 source export, frozen native build and archive preparation. Never publishes."""
import argparse
from datetime import date
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile

# Keep verified extracted sources unchanged when invoking their own tools.
sys.dont_write_bytecode = True

from package_legal import product_version, write_manifest, public_documentation
from verify_artwork_rights import check_artwork_rights
from verify_v03_package import reject_private_artwork
from windows_signing import EXECUTABLES, verify_directory

ROOT = Path(__file__).resolve().parents[1]
LABEL = 'Unsigned public release — publisher unverified'
MAX_ARCHIVE_BYTES = 2 * 1024**3
MAX_ARCHIVE_FILES = 100000
MODES = ('internal-unsigned', 'unsigned-review', 'public-unsigned', 'public-signed')


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def encode(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + '\n').encode()


def save(root, name, raw):
    target = root / safe(name)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(raw)


def safe(name):
    path = PurePosixPath(name)
    reserved = {'con', 'prn', 'aux', 'nul'} | {f'{k}{n}' for k in ('com', 'lpt') for n in range(1, 10)}
    if (not name or path.is_absolute() or path.as_posix() != name or '\\' in name or ':' in name
            or any(p in ('.', '..') or p.endswith((' ', '.')) or p.split('.')[0].casefold() in reserved for p in name.split('/'))):
        raise ValueError('Unsafe member: ' + name)
    return name


def privacy(name, raw):
    if '.git' in PurePosixPath(name).parts or (not name.startswith('cargo-vendor/') and (any(p in ('private', 'target', '__pycache__') for p in PurePosixPath(name).parts) or name == 'AGENTS.md' or name.startswith('docs/validation/'))):
        raise ValueError('Private/build member: ' + name)
    # Third-party source fixtures may contain illustrative user paths. They are
    # admitted only after Cargo checksum verification and never rewritten.
    if not name.startswith('cargo-vendor/'):
        patterns = (rb'/(?:home|media)/[A-Za-z0-9][A-Za-z0-9_.-]*/',
                    rb'[C-Z]:[/\\](?:Users[/\\][^/\\\s]+|SortSizeLab|BunnyCI|BunnyIntegration)',
                    rb'(?:github_pat|gh[pousr])_[A-Za-z0-9]{10,}', rb'-----BEGIN (?:OPENSSH |RSA |EC )?PRIVATE KEY-----')
        for pattern in patterns:
            if re.search(pattern, raw) or re.search(pattern, raw.replace(b'\0', b'')):
                raise ValueError('Private path or credential marker: ' + name)
    reject_private_artwork(name, raw)


def files(root):
    result = {}
    for p in sorted(root.rglob('*')):
        if p.is_symlink():
            raise ValueError('Links forbidden')
        if p.is_file():
            n = safe(p.relative_to(root).as_posix())
            raw = p.read_bytes()
            privacy(n, raw)
            result[n] = raw
    return result


def verify_files(data):
    rows = json.loads(data['FILES.json'])
    names = [r['path'] for r in rows]
    if len(names) != len(set(names)) or set(names) != set(data) - {'FILES.json', 'SHA256SUMS'}:
        raise ValueError('Manifest membership mismatch')
    for r in rows:
        if sha(data[r['path']]) != r['sha256'] or len(data[r['path']]) != r['bytes']:
            raise ValueError('Manifest hash/size mismatch')
    sums = [line.split('  ', 1) for line in data['SHA256SUMS'].decode().splitlines()]
    if len(sums) != len(data) - 1 or {n for _, n in sums} != set(data) - {'SHA256SUMS'} or any(sha(data[n]) != h for h, n in sums):
        raise ValueError('Checksum mismatch')


def archive(root, destination):
    data = files(root)
    verify_files(data)
    if destination.suffix == '.zip':
        with zipfile.ZipFile(destination, 'x', zipfile.ZIP_DEFLATED, compresslevel=9) as z:
            for name, raw in data.items():
                i = zipfile.ZipInfo(root.name + '/' + name, (1980, 1, 1, 0, 0, 0))
                i.create_system = 3
                i.external_attr = (0o100755 if name in ('bunny-and-size', 'bunny-and-size-cli', 'sortsize') else 0o100644) << 16
                z.writestr(i, raw, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    else:
        with destination.open('xb') as output, gzip.GzipFile(filename='', mode='wb', fileobj=output, mtime=0, compresslevel=9) as gz:
            with tarfile.open(fileobj=gz, mode='w', format=tarfile.PAX_FORMAT) as t:
                for name, raw in data.items():
                    i = tarfile.TarInfo(root.name + '/' + name)
                    i.size = len(raw)
                    i.mode = 0o755 if name in ('bunny-and-size', 'bunny-and-size-cli', 'sortsize') else 0o644
                    t.addfile(i, io.BytesIO(raw))
    return dict(filename=destination.name, sha256=sha(destination.read_bytes()), bytes=destination.stat().st_size)


def read_archive(path):
    entries = []
    total = 0
    def admit(name, size):
        nonlocal total
        safe(name)
        total += size
        if total > MAX_ARCHIVE_BYTES or len(entries) >= MAX_ARCHIVE_FILES:
            raise ValueError('Archive exceeds admission budget')
    if path.suffix == '.zip':
        with zipfile.ZipFile(path) as z:
            for i in z.infolist():
                if i.is_dir() or (stat.S_IFMT(i.external_attr >> 16) and not stat.S_ISREG(i.external_attr >> 16)):
                    raise ValueError('Nonregular member')
                admit(i.filename, i.file_size)
                entries.append((i.filename, z.read(i)))
    else:
        with tarfile.open(path) as t:
            for i in t:
                if not i.isfile():
                    raise ValueError('Nonregular member')
                admit(i.name, i.size)
                entries.append((i.name, t.extractfile(i).read()))
    seen, roots, data = set(), set(), {}
    for name, raw in entries:
        safe(name)
        if name.casefold() in seen or '/' not in name:
            raise ValueError('Duplicate/rootless member')
        seen.add(name.casefold())
        root, relative = name.split('/', 1)
        roots.add(root)
        privacy(relative, raw)
        data[relative] = raw
    if len(roots) != 1 or not next(iter(roots)).startswith('Bunny-and-Size-v0.4.'):
        raise ValueError('Unexpected archive root')
    verify_files(data)
    return data


def authorize(record, info):
    if not record:
        raise ValueError('Separate owner authorization required; public gate closed')
    raw = Path(record).read_bytes()
    a = json.loads(raw)
    if a.get('schema') == 2:
        platform = 'windows-x86_64' if set(info['binaries']) == set(EXECUTABLES) else 'linux-x86_64'
        names = set(EXECUTABLES if platform == 'windows-x86_64' else (n[:-4] for n in EXECUTABLES))
        expected = dict(schema=2, approved=True, owner='tazmen3', mode='public-unsigned',
                        scope='local-package-preparation', distribution_approved=False,
                        version=info['version'], source_commit=info['source_commit'],
                        source_archive_sha256=info['source_archive']['sha256'],
                        built_from_commit=info['built_from_commit'],
                        build_inputs_sha256=info['build_inputs_sha256'])
        qualified = a.get('qualified_binaries', {})
        if (any(type(a.get(k)) is not type(v) or a.get(k) != v for k, v in expected.items())
                or set(info['binaries']) != names
                or not isinstance(qualified, dict)
                or set(qualified) != {'windows-x86_64', 'linux-x86_64'}
                or any(not isinstance(v, dict) for v in qualified.values())
                or set(qualified['windows-x86_64']) != set(EXECUTABLES)
                or set(qualified['linux-x86_64']) != {n[:-4] for n in EXECUTABLES}
                or qualified[platform] != {n: r['sha256'] for n, r in info['binaries'].items()}
                or not a.get('decision_reference') or not a.get('date')):
            raise ValueError('Local preparation authorization does not match exact inputs/scope')
        return dict(sha256=sha(raw), **a)
    raise ValueError('Preparation requires schema 2 local-package-preparation; release approval is external')


def export(output, legal_root):
    if subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=all'], cwd=ROOT).strip():
        raise ValueError('Export requires a clean committed checkout')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    selected = json.loads((ROOT / 'packaging/v04-public-files.json').read_text())
    if len(selected) != len(set(selected)):
        raise ValueError('Duplicate selection')
    version = product_version(ROOT)
    review = json.loads((ROOT / 'packaging/v04-dependency-review.json').read_text())
    if any(sha((ROOT / n).read_bytes()) != h for n, h in review['lock_sha256'].items()):
        raise ValueError('Dependency graph changed; legal review required')
    check_artwork_rights(ROOT, False)
    output.mkdir(parents=True, exist_ok=False)
    source = output / f'Bunny-and-Size-v{version}-source'
    source.mkdir()
    blob_rows = []
    for name in selected:
        safe(name)
        raw = subprocess.check_output(['git', 'show', revision + ':' + name], cwd=ROOT)
        privacy(name, raw)
        save(source, name, raw)
        blob_rows.append(dict(path=name, sha256=sha(raw)))
    # Exact reviewed legal texts from the already qualified V0.4 inputs. No
    # historical executable, build path, evidence or BUILDINFO is copied.
    legal = json.loads((ROOT / 'packaging/v04-legal-files.json').read_text())
    for platform, rows in legal.items():
        for row in rows:
            raw = (legal_root / platform / safe(row['path'])).read_bytes()
            if sha(raw) != row['sha256']:
                raise ValueError('Reviewed legal payload changed: ' + row['path'])
            save(source, 'distribution-legal/' + platform + '/' + row['path'], raw)
    vendor = source / 'cargo-vendor'
    subprocess.run(['cargo', 'vendor', '--locked', '--offline', '--sync', 'spikes/slint/Cargo.toml', str(vendor)], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    save(source, '.cargo/config.toml', b'[source.crates-io]\nreplace-with="distribution-vendor"\n[source.distribution-vendor]\ndirectory="cargo-vendor"\n[net]\noffline=true\n')
    info = dict(schema=1, version=version, source_commit=revision, audience='public-prerelease', intended_distribution='public-unsigned',
                release_approval_required='external-final-assets', public_release=False,
                distribution_approved=False,
                product_license='GPL-3.0-only', selection=blob_rows,
                legal_manifest_sha256=sha((ROOT / 'packaging/v04-legal-files.json').read_bytes()),
                lock_sha256={n: sha((source / n).read_bytes()) for n in ['Cargo.lock', 'spikes/slint/Cargo.lock']},
                cargo_vendor_packages=len(list(vendor.iterdir())), D1='UNRESOLVED #139')
    save(source, 'README.md', (source / 'docs/release/v0.4/SOURCE.md').read_bytes())
    public_documentation(source)
    for row in info['selection']:
        row['git_blob_sha256'] = row['sha256']
        row['sha256'] = sha((source / row['path']).read_bytes())
    save(source, 'SOURCE_SELECTION.json', encode(info))
    write_manifest(source)
    record = archive(source, output / (source.name + '.tar.gz'))
    save(output, 'SOURCE_ARCHIVE.json', encode(record))
    return dict(source_commit=revision, **record)


def source_info(source):
    verify_files(files(source))
    info = json.loads((source / 'SOURCE_SELECTION.json').read_text())
    for row in info['selection']:
        if sha((source / row['path']).read_bytes()) != row['sha256']:
            raise ValueError('Selected source changed')
    return info


def build(source, output):
    info = source_info(source)
    output.mkdir(parents=True, exist_ok=False)
    target = 'x86_64-pc-windows-msvc' if os.name == 'nt' else 'x86_64-unknown-linux-gnu'
    env = dict(os.environ)
    for n in list(env):
        if n.startswith(('CARGO_', 'SLINT_', 'RUSTFLAGS', 'RUST_FONTCONFIG')) or n == 'RUSTUP_TOOLCHAIN':
            env.pop(n)
    home = output.resolve() / 'cargo-home'
    home.mkdir()
    env.update(CARGO_HOME=str(home), CARGO_TARGET_DIR=str(output.resolve() / 'target'), CARGO_NET_OFFLINE='true',
               CARGO_BUILD_JOBS='2', RUSTUP_TOOLCHAIN='1.99.0',
               RUSTFLAGS=f'--remap-path-prefix={source.resolve()}=/src --remap-path-prefix={output.resolve()}=/build')
    tool = subprocess.check_output(['rustc', '-Vv'], env=env, text=True)
    if 'release: 1.99.0' not in tool or 'host: ' + target not in tool:
        raise ValueError('Pinned native Rust 1.99.0 required')
    # Rust std embeds its own compiler source prefix, not the lab. MSVC /Brepro
    # removes wall-clock linker timestamps and deterministic debug identifiers.
    if os.name == 'nt':
        env['RUSTFLAGS'] += ' -C link-arg=/Brepro'
    rows = []
    for label, args in [('cli', ['-p', 'sortsize-cli', '--bins']),
                        ('gui', ['--manifest-path', 'spikes/slint/Cargo.toml', '--bin', 'bunny-and-size'])]:
        cmd = ['cargo', 'build', '--frozen', '--release', '--target', target] + args
        with (output / (label + '.log')).open('wb') as log:
            result = subprocess.run(cmd, cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT)
        rows.append(dict(component=label, exit_code=result.returncode))
        if result.returncode:
            raise ValueError('Clean build failed: ' + label)
    names = EXECUTABLES if os.name == 'nt' else tuple(n[:-4] for n in EXECUTABLES)
    binaries = {n: dict(sha256=sha((output / 'target' / target / 'release' / n).read_bytes())) for n in names}
    record = dict(source_commit=info['source_commit'], version=info['version'], source_selection_sha256=sha((source / 'SOURCE_SELECTION.json').read_bytes()), target=target, rustc=tool, commands=rows, binaries=binaries)
    save(output, 'BUILD.json', encode(record))
    return record


def compiler_inputs(rows):
    return {r['path']: r['sha256'] for r in rows if r['path'].startswith(('apps/', 'crates/', 'spikes/', 'assets/', 'vendor/', 'cargo-vendor/', '.cargo/')) or r['path'] in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'packaging/windows/version-resource.rs')}


def build_inputs(source):
    return compiler_inputs(json.loads((source / 'FILES.json').read_text(encoding='utf-8')))


def package(source, build_dir, source_archive, output, mode, authorization=None, built_source=None):
    if mode not in MODES:
        raise ValueError('Unknown distribution mode')
    info = source_info(source)
    qualified = source if built_source is None else built_source
    built_info = info if built_source is None else source_info(qualified)
    if build_inputs(source) != build_inputs(qualified):
        raise ValueError('Qualified build inputs differ from corresponding sources')
    buildinfo = json.loads((build_dir / 'BUILD.json').read_text())
    if (buildinfo['source_commit'] != built_info['source_commit'] or buildinfo['source_selection_sha256'] != sha((qualified / 'SOURCE_SELECTION.json').read_bytes())):
        raise ValueError('Build provenance mismatch')
    platform = 'windows-x86_64' if os.name == 'nt' else 'linux-x86_64'
    if os.name != 'nt' and mode not in ('unsigned-review', 'internal-unsigned', 'public-unsigned'):
        raise ValueError('Windows distribution mode on Linux')
    # Bind the source archive to the exact verified extracted tree.
    if read_archive(source_archive) != files(source):
        raise ValueError('Corresponding source archive differs')
    info = dict(version=info['version'], source_commit=info['source_commit'], D1='UNRESOLVED #139',
                public_release=False, redistribution_ready=False, audience='review-only',
                built_from_commit=buildinfo['source_commit'],
                build_inputs_sha256=sha(json.dumps(build_inputs(source), sort_keys=True).encode()),
                windows_distribution=mode,
                source_archive=dict(filename=source_archive.name, sha256=sha(source_archive.read_bytes())),
                binaries={}, build=buildinfo)
    release = build_dir / 'target' / buildinfo['target'] / 'release'
    names = EXECUTABLES if os.name == 'nt' else tuple(n[:-4] for n in EXECUTABLES)
    for n in names:
        raw = (release / n).read_bytes()
        privacy(n, raw)
        for prefix in (source, qualified, build_dir):
            for variant in {str(prefix.resolve()), str(prefix.resolve()).replace('\\', '/'), str(prefix.resolve()).replace('/', '\\')}:
                if any(variant.lower().encode(codec) in raw.lower() for codec in ('utf-8', 'utf-16le')):
                    raise ValueError('Build/source prefix embedded in executable')
        if sha(raw) != buildinfo['binaries'][n]['sha256']:
            raise ValueError('Build binary changed')
        if os.name == 'nt':
            pe = struct.unpack_from('<I', raw, 0x3c)[0]
            if raw[:2] != b'MZ' or raw[pe:pe+4] != b'PE\0\0' or struct.unpack_from('<H', raw, pe+4)[0] != 0x8664 or struct.unpack_from('<H', raw, pe+24+68)[0] != (2 if n == 'bunny-and-size.exe' else 3):
                raise ValueError('Unexpected PE architecture/subsystem')
        elif raw[:6] != b'\x7fELF\x02\x01' or struct.unpack_from('<H', raw, 18)[0] != 62:
            raise ValueError('Expected ELF64')
        info['binaries'][n] = dict(sha256=sha(raw), bytes=len(raw))
    if mode == 'public-unsigned':
        info['authorization'] = authorize(authorization, info)
        info['audience'] = LABEL
        info.update(preparation_approved=True, distribution_approved=False,
                    status='READY FOR OWNER RELEASE APPROVAL',
                    packaging_validated=True, intended_audience='public-prerelease',
                    release_approval_required='external-final-assets')
    elif mode == 'public-signed':
        info['redistribution_ready'] = True
        info['audience'] = 'Signed public release — native verification required'
    elif authorization:
        raise ValueError('Authorization only accepted in public-unsigned mode')
    output.mkdir(parents=True, exist_ok=False)
    folder = output / f"Bunny-and-Size-v{info['version']}-{mode}-{platform}"
    folder.mkdir()
    for n in names:
        save(folder, n, (release / n).read_bytes())
        (folder / n).chmod(0o755)
    for row in json.loads((source / 'packaging/v04-legal-files.json').read_text())[platform]:
        save(folder, row['path'], (source / 'distribution-legal' / platform / row['path']).read_bytes())
    if os.name != 'nt':
        save(folder, 'share/applications/bunny-and-size.desktop', (source / 'packaging/linux/bunny-and-size.desktop').read_bytes())
        for size in (16, 24, 32, 48, 64, 128, 256, 512):
            save(folder, f'share/icons/hicolor/{size}x{size}/apps/bunny-and-size.png', (source / f'assets/branding/icons/bunny-and-size-{size}.png').read_bytes())
    for name in ['LICENSE', 'CREDITS.md', 'BRANDING.md']:
        save(folder, name, (source / name).read_bytes())
    for name in ['USAGE.md', 'LANGUAGES.md', 'WINDOWS_SECURITY.md', 'RESERVATIONS.md']:
        origin = source / ('docs/v0.4/' + name if name in ('USAGE.md', 'LANGUAGES.md') else 'docs/release/v0.4/' + name)
        save(folder, name, origin.read_bytes())
    save(folder, 'README.md', (f"# Bunny and Size {info['version']}\n\n" + (LABEL if mode == 'public-unsigned' else 'NOT AUTHORIZED FOR PUBLICATION — ' + mode) + '\n\nSee USAGE.md, WINDOWS_SECURITY.md, RESERVATIONS.md, FILES.json and SHA256SUMS.\nCorresponding sources: ' + source_archive.name + '\n').encode())
    if info.get('preparation_approved'):
        with (folder / 'README.md').open('ab') as stream:
            stream.write(b'\nPublic unsigned prerelease package; packaging validated. Publication requires an external owner decision on all seven final asset names, sizes and SHA-256.\ndistribution_approved=false; public_release=false.\n')
    if os.name == 'nt':
        info['windows_signing'] = verify_directory(folder, info['version'], mode == 'public-signed', unsigned=mode != 'public-signed')
    save(folder, 'BUILDINFO.json', encode(info))
    write_manifest(folder)
    dest = output / (folder.name + ('.zip' if os.name == 'nt' else '.tar.gz'))
    result = archive(folder, dest)
    verify(dest, mode, authorization)
    save(output, 'SHA256SUMS', (result['sha256'] + '  ' + dest.name + '\n').encode())
    return result


def verify_disclosures(data, windows):
    required_docs = ['LICENSE', 'CREDITS.md', 'BRANDING.md', 'README.md', 'USAGE.md', 'LANGUAGES.md', 'WINDOWS_SECURITY.md', 'RESERVATIONS.md']
    legal = json.loads((ROOT / 'packaging/v04-legal-files.json').read_text())['windows-x86_64' if windows else 'linux-x86_64']
    for row in legal:
        if row['path'] not in data or sha(data[row['path']]) != row['sha256']:
            raise ValueError('Missing/changed legal payload')
    if any(n not in data for n in required_docs):
        raise ValueError('Missing distribution documentation')
    if not windows:
        expected = {'share/applications/bunny-and-size.desktop': ROOT / 'packaging/linux/bunny-and-size.desktop'}
        expected.update({f'share/icons/hicolor/{size}x{size}/apps/bunny-and-size.png': ROOT / f'assets/branding/icons/bunny-and-size-{size}.png' for size in (16, 24, 32, 48, 64, 128, 256, 512)})
        if any(data.get(n) != p.read_bytes() for n, p in expected.items()):
            raise ValueError('Linux desktop resources missing/changed')
    if data['WINDOWS_SECURITY.md'] != (ROOT / 'docs/release/v0.4/WINDOWS_SECURITY.md').read_bytes():
        raise ValueError('Windows security disclosure mismatch')


def verify(path, mode='public-signed', authorization=None):
    if mode not in MODES:
        raise ValueError('Unknown distribution mode')
    data = read_archive(path)
    info = json.loads(data['BUILDINFO.json'])
    actual = info.get('windows_distribution')
    if actual != mode:
        raise ValueError('Distribution mode mismatch')
    if mode == 'public-unsigned':
        approval = authorize(authorization, info)
        if approval != info.get('authorization') or info.get('audience') != LABEL:
            raise ValueError('Public unsigned authorization mismatch')
    local = mode == 'public-unsigned'
    if local and (info.get('distribution_approved') is not False or info.get('public_release') is not False
                  or info.get('redistribution_ready') is not False or info.get('preparation_approved') is not True
                  or info.get('packaging_validated') is not True
                  or info.get('intended_audience') != 'public-prerelease'
                  or info.get('release_approval_required') != 'external-final-assets'
                  or info.get('status') != 'READY FOR OWNER RELEASE APPROVAL'):
        raise ValueError('Local preparation cannot approve distribution; invalid packaging metadata')
    if mode == 'public-signed' and not info.get('redistribution_ready'):
        raise ValueError('Public distribution readiness missing')
    verify_disclosures(data, path.suffix == '.zip')
    names = EXECUTABLES if path.suffix == '.zip' else tuple(n[:-4] for n in EXECUTABLES)
    if set(info['binaries']) != set(names):
        raise ValueError('Executable membership mismatch')
    for n in names:
        if (sha(data[n]) != info['binaries'][n]['sha256']
                or len(data[n]) != info['binaries'][n]['bytes']
                or info['build']['binaries'][n]['sha256'] != info['binaries'][n]['sha256']):
            raise ValueError('Executable provenance mismatch')
    if (info['build']['source_commit'] != info['built_from_commit']
            or info['build']['version'] != info['version']):
        raise ValueError('Build provenance mismatch')
    if path.suffix == '.zip':
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            for n, raw in data.items():
                save(root, n, raw)
            verify_directory(root, info['version'], mode == 'public-signed', unsigned=mode != 'public-signed')
    return dict(sha256=sha(path.read_bytes()), mode=mode, source_commit=info['source_commit'],
                status='READY FOR OWNER RELEASE APPROVAL' if local else 'TECHNICALLY VERIFIED',
                public_authorized=False,
                preparation_approved=local, distribution_approved=False if local else None)


def asset_names(version, mode):
    if not re.fullmatch(r'0\.4\.\d+-rc\.\d+', version) or mode not in ('public-unsigned', 'public-signed'):
        raise ValueError('Invalid public prerelease version/mode')
    prefix = 'Bunny-and-Size-v' + version
    return {prefix + '-source.tar.gz', prefix + '-' + mode + '-linux-x86_64.tar.gz',
            prefix + '-' + mode + '-windows-x86_64.zip', 'OWNER_PREPARATION_AUTHORIZATION.json',
            'PROVENANCE.json', 'RELEASE_NOTES.md', 'SHA256SUMS'}


def release_authorization(record, context, *, allow_synthetic=False):
    """Validate an EXTERNAL human decision; never serialize it into the assets.

    The caller supplies a trusted transcription of the owner's decision. JSON
    is an audit record, not a signature or proof of the author's identity.
    Synthetic fixtures are refused by the production CLI and cannot grant GO.
    """
    if record is None:
        raise ValueError('External final owner release approval required; publication blocked')
    raw = Path(record).read_bytes()
    a = json.loads(raw)
    expected = dict(schema=3, approved=True, owner='tazmen3', scope='final-asset-publication',
                    **context)
    if any(type(a.get(k)) is not type(v) or a.get(k) != v for k, v in expected.items()):
        raise ValueError('Final owner approval does not match exact assets/source/provenance/mode')
    if type(a.get('synthetic')) is not bool:
        raise ValueError('Explicit synthetic=false required for a real decision')
    if a['synthetic'] and not allow_synthetic:
        raise ValueError('Synthetic fixture is not owner release approval')
    reference = a.get('decision_reference')
    if not isinstance(reference, str) or not reference.strip():
        raise ValueError('Owner decision reference required')
    try:
        date.fromisoformat(a['date'])
    except (ValueError, KeyError, TypeError):
        raise ValueError('Owner decision date must be ISO YYYY-MM-DD') from None
    return dict(status='SYNTHETIC TEST ONLY' if a['synthetic'] else 'AUTHORIZED FOR PUBLICATION',
                public_authorized=not a['synthetic'], authorization_validated=True,
                distribution_approved=not a['synthetic'], decision_reference=reference,
                date=a['date'], approval_sha256=sha(raw))


def verify_delivery(delivery, version, mode='public-unsigned', release_approval=None, *, require_publication=False, allow_synthetic=False):
    """Check all seven immutable assets. Windows native checks remain mandatory.

    Without approval this reports technical readiness; --require-publication
    fails closed. This module intentionally has no publishing operation.
    """
    delivery = Path(delivery).resolve()
    expected = asset_names(version, mode)
    paths = list(delivery.iterdir())
    if ({p.name for p in paths} != expected
            or any(p.is_symlink() or not p.is_file() for p in paths)):
        raise ValueError('Delivery must contain exactly seven regular final assets')
    if release_approval is not None and Path(release_approval).resolve().is_relative_to(delivery):
        raise ValueError('Final release approval must remain external to delivery')
    rows = [dict(filename=p.name, bytes=p.stat().st_size, sha256=sha(p.read_bytes()))
            for p in sorted(paths)]
    lookup = {r['filename']: r for r in rows}
    sums = [line.split('  ', 1) for line in (delivery / 'SHA256SUMS').read_text(encoding='utf-8').splitlines()]
    if (len(sums) != 6 or {n for _, n in sums} != expected - {'SHA256SUMS'}
            or any(lookup[n]['sha256'] != h for h, n in sums)):
        raise ValueError('Delivery SHA256SUMS mismatch')
    prefix = 'Bunny-and-Size-v' + version
    source_name = prefix + '-source.tar.gz'
    source = read_archive(delivery / source_name)
    if any('OWNER_RELEASE_APPROVAL' in n.upper() or 'OWNER_PUBLICATION_AUTHORIZATION' in n.upper() for n in source):
        raise ValueError('Release approval must be external to archives')
    selection = json.loads(source['SOURCE_SELECTION.json'])
    if (selection['version'] != version or selection.get('audience') != 'public-prerelease'
            or selection.get('intended_distribution') != mode
            or selection.get('release_approval_required') != 'external-final-assets'
            or selection.get('distribution_approved') is not False
            or selection.get('public_release') is not False):
        raise ValueError('Corresponding source version/destination mismatch')
    for row in selection['selection']:
        if sha(source[row['path']]) != row['sha256']:
            raise ValueError('Selected corresponding source changed')
    source_rows = json.loads(source['FILES.json'])
    inputs = compiler_inputs(source_rows)
    inputs_sha = sha(json.dumps(inputs, sort_keys=True).encode())
    auth = delivery / 'OWNER_PREPARATION_AUTHORIZATION.json'
    qualified, built_from = {}, set()
    for platform, ext in [('linux-x86_64', '.tar.gz'), ('windows-x86_64', '.zip')]:
        name = prefix + '-' + mode + '-' + platform + ext
        path = delivery / name
        verify(path, mode, auth if mode == 'public-unsigned' else None)
        data = read_archive(path)
        info = json.loads(data['BUILDINFO.json'])
        if (info['version'] != version or info['source_commit'] != selection['source_commit']
                or info['source_archive'] != dict(filename=source_name, sha256=lookup[source_name]['sha256'])
                or info['build_inputs_sha256'] != inputs_sha):
            raise ValueError('Corresponding sources/build inputs mismatch')
        # No final decision or approval record may be hidden in archive members.
        if any('OWNER_RELEASE_APPROVAL' in n.upper() or 'OWNER_PUBLICATION_AUTHORIZATION' in n.upper() for n in data):
            raise ValueError('Release approval must be external to archives')
        qualified[platform] = info['binaries']
        built_from.add(info['built_from_commit'])
    if len(built_from) != 1:
        raise ValueError('Inconsistent built_from_commit')
    context = dict(version=version, mode=mode, source_commit=selection['source_commit'],
                   source_archive_sha256=lookup[source_name]['sha256'],
                   built_from_commit=next(iter(built_from)), build_inputs_sha256=inputs_sha,
                   qualified_binaries=qualified, assets=rows)
    provenance = json.loads((delivery / 'PROVENANCE.json').read_bytes())
    for key in ('version', 'mode', 'source_commit', 'built_from_commit', 'build_inputs_sha256', 'qualified_binaries'):
        if provenance.get(key) != context[key]:
            raise ValueError('Delivery provenance mismatch: ' + key)
    if (provenance.get('source_selection_sha256') != sha(source['SOURCE_SELECTION.json'])
            or provenance.get('compiler_input_count') != len(inputs)
            or provenance.get('distribution_approved') is not False
            or provenance.get('status') != 'READY FOR OWNER RELEASE APPROVAL'):
        raise ValueError('Delivery provenance readiness mismatch')
    result = dict(status='READY FOR OWNER RELEASE APPROVAL', public_authorized=False,
                  distribution_approved=False, packaging_validated=True, context=context)
    if release_approval is not None or require_publication:
        result.update(release_authorization(release_approval, context, allow_synthetic=allow_synthetic))
    # Detect changes during qualification, including the seven standalone files.
    if any(p.stat().st_size != lookup[p.name]['bytes'] or sha(p.read_bytes()) != lookup[p.name]['sha256'] for p in paths):
        raise ValueError('Assets changed during verification')
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='action', required=True)
    e = sub.add_parser('export')
    e.add_argument('--output', type=Path, required=True)
    e.add_argument('--legal-root', type=Path, required=True)
    b = sub.add_parser('build')
    b.add_argument('--source', type=Path, required=True)
    b.add_argument('--output', type=Path, required=True)
    a = sub.add_parser('package')
    a.add_argument('--source', type=Path, required=True)
    a.add_argument('--build', type=Path, required=True)
    a.add_argument('--built-source', type=Path, help='Previously built export; every compiler input must match the final sources exactly')
    a.add_argument('--source-archive', type=Path, required=True)
    a.add_argument('--output', type=Path, required=True)
    a.add_argument('--mode', choices=MODES, default='public-signed')
    a.add_argument('--authorization', type=Path)
    v = sub.add_parser('verify')
    v.add_argument('archive', type=Path)
    v.add_argument('--mode', choices=MODES, default='public-signed')
    v.add_argument('--authorization', type=Path)
    gate = sub.add_parser('verify-delivery', help='Technical readiness or external final publication gate; never publishes')
    gate.add_argument('--delivery', type=Path, required=True)
    gate.add_argument('--version', required=True)
    gate.add_argument('--mode', choices=('public-unsigned', 'public-signed'), default='public-signed')
    gate.add_argument('--release-approval', type=Path)
    gate.add_argument('--require-publication', action='store_true')
    args = p.parse_args()
    if args.action == 'export':
        result = export(args.output, args.legal_root)
    elif args.action == 'build':
        result = build(args.source.resolve(), args.output.resolve())
    elif args.action == 'package':
        result = package(args.source.resolve(), args.build.resolve(), args.source_archive.resolve(), args.output.resolve(), args.mode, args.authorization, args.built_source.resolve() if args.built_source else None)
    elif args.action == 'verify-delivery':
        result = verify_delivery(args.delivery, args.version, args.mode, args.release_approval, require_publication=args.require_publication)
    else:
        result = verify(args.archive, args.mode, args.authorization)
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
