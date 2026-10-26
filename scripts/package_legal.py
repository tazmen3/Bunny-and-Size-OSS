"""Entrées légales communes aux deux packagers ; aucune dépendance externe."""
import hashlib
import io
import tarfile
import json
from pathlib import Path
import subprocess
import tomllib


def sha(data):
    return hashlib.sha256(data).hexdigest()


def legal_inputs(root):
    entries = json.loads((root / 'packaging/windows/source-licences/index.json').read_text(encoding='utf-8'))
    for entry in entries:
        entry['path'] = 'packaging/windows/source-licences/' + entry['file']
        entry['kind'] = 'primary'
        entry['scope'] = 'source-resolution-only; not enabled in executables'
    entries += json.loads((root / 'packaging/source-licences/index.json').read_text(encoding='utf-8'))
    for entry in entries:
        relative = Path(entry['path'])
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('Chemin légal non relatif')
        if sha((root / relative).read_bytes()) != entry['sha256']:
            raise ValueError('Texte légal modifié : ' + entry['path'])
    return entries


def legal_texts(root, entries, key, kinds):
    return [(f'licences/cargo/source-extra/{key[0]}-{key[1]}/' + Path(e['path']).name,
             (root / e['path']).read_bytes()) for e in entries
            if (e['name'], e['version']) == key and e['kind'] in kinds]


def check_audit(root, audit, audit_commit):
    subprocess.run(['git', 'merge-base', '--is-ancestor', audit_commit, 'HEAD'], cwd=audit, check=True)
    subprocess.run(['git', 'diff', '--exit-code', audit_commit, 'HEAD', '--',
                    'docs/validation/licences-v0.1', 'docs/validation/licences-v0.1-reconciliation'],
                   cwd=audit, check=True, stdout=subprocess.PIPE)
    dirty = subprocess.check_output(['git', 'status', '--porcelain'], cwd=audit)
    if dirty.strip():
        raise ValueError('Le corpus audité doit être dans un checkout propre.')
    dirty = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=all'], cwd=root)
    if dirty.strip():
        raise ValueError('Le checkout complet de packaging doit être propre (index et fichiers non suivis inclus).')
    # Git peut déclarer propre un checkout converti en CRLF. Les blobs fournis
    # comme source correspondant doivent être exactement les entrées de build.
    data = subprocess.check_output(['git', 'archive', 'HEAD', '--',
        'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'LICENSE', 'apps',
        'crates', 'spikes/slint', 'assets/branding', 'packaging', 'scripts/package_legal.py',
        'scripts/package_linux.py', 'scripts/package_windows.py',
        'scripts/package-linux.sh', 'scripts/package-windows.ps1',
        'docs/release/v0.3'], cwd=root)
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        for member in archive:
            if member.isfile() and (root / member.name).read_bytes() != archive.extractfile(member).read():
                raise ValueError('Entrée de build différente du blob Git (CRLF ?) : ' + member.name)


def product_identity(root, commit):
    data = subprocess.check_output(['git', 'ls-tree', '-r', commit, '--',
                                   'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'LICENSE',
                                   'apps', 'crates', 'spikes/slint', 'assets/branding', 'vendor',
                                   'packaging/windows/version-resource.rs'], cwd=root)
    return sha(data)


def write_manifest(directory):
    rows = [{'path': p.relative_to(directory).as_posix(), 'bytes': p.stat().st_size,
             'sha256': sha(p.read_bytes())} for p in sorted(directory.rglob('*'))
            if p.is_file() and p.relative_to(directory).as_posix() not in ('FILES.json', 'SHA256SUMS')]
    (directory / 'FILES.json').write_text(json.dumps(rows, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    data = ''.join(f"{r['sha256']}  {r['path']}\n" for r in rows)
    data += f"{sha((directory / 'FILES.json').read_bytes())}  FILES.json\n"
    (directory / 'SHA256SUMS').write_text(data, encoding='utf-8')


def product_version(root):
    main = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    gui = tomllib.loads((root / "spikes/slint/Cargo.toml").read_text())["package"]["version"]
    if main != gui:
        raise ValueError("Versions des deux workspaces différentes")
    return main


def check_registry_lock(data, reference):
    # Les crates internes évoluent avec le produit ; le corpus légal ne vaut
    # que pour des archives externes strictement identiques (checksum compris).
    def registry(raw):
        return sorted((p for p in tomllib.loads(raw.decode())["package"] if p.get("source")),
                      key=lambda p: (p["name"], p["version"]))
    current, original = registry(data), registry(reference)
    if current == original:
        return
    # Exact, versioned review of the pre-existing v0.3 i18n build graph.
    # Future lockfile changes fail closed until a new review is recorded.
    review = json.loads((Path(__file__).resolve().parents[1] / 'packaging/v03-registry-review.json').read_text())
    digest = lambda rows: sha(json.dumps(rows, sort_keys=True, separators=(',', ':')).encode())
    if not any(row['reference_registry_sha256'] == digest(original) and
               row['reviewed_registry_sha256'] == digest(current) for row in review['locks']):
        raise ValueError("Dépendances externes différentes du corpus légal : audit requis")


def local_backend_notice(root):
    subprocess.run([__import__('sys').executable,
                    str(root / 'scripts/verify_redraw_vendor.py')], cwd=root, check=True, stdout=subprocess.PIPE)
    folder = root / 'vendor/i-slint-backend-winit-1.18.1'
    files = [('licences/vendor/i-slint-backend-winit-1.18.1/' + p.relative_to(folder).as_posix(), p.read_bytes())
             for p in sorted((folder / 'LICENSES').iterdir()) if p.is_file()]
    files.append(('licences/vendor/provenance.json', (root / 'vendor/provenance.json').read_bytes()))
    row = dict(name='i-slint-backend-winit', version='1.18.1', license_selected='GPL-3.0-only',
               roles=['gui:runtime', 'gui:production/build'], source='vendored with Windows #94 patch',
               texts=[dict(path=name, sha256=sha(raw)) for name, raw in files])
    text = 'i-slint-backend-winit 1.18.1 + #94 | GPL-3.0-only | vendored GUI runtime/build\n'
    text += '\n'.join('  ' + name for name, _ in files)
    return row, files, text


def public_documentation(directory):
    """Render only project distribution docs; never edit third-party notices.

    Private evidence and planning documents stay private. Missing internal links
    become explicit labels rather than dangling links to unshipped documents.
    """
    import re
    from urllib.parse import unquote, urlsplit
    names = ['README.md', 'USER_GUIDE.md', 'COMPATIBILITY.md', 'RESERVATIONS.md',
             'CREDITS.md', 'BRANDING.md', 'docs/ARTWORK_LICENSE_DECISION.md', 'MIGRATION_V03.md', 'docs/CLI.md',
             'docs/GUI.md', 'docs/ENTRIES_EXPORT.md', 'spikes/slint/README.md']
    root = directory.resolve()
    for name in names:
        path = directory / name
        if not path.is_file():
            continue
        replaced = 0
        def link(match):
            nonlocal replaced
            label, target = match.group(1), match.group(2)
            relative = unquote(target.split('#', 1)[0])
            if not relative or urlsplit(target).scheme or target.startswith('//'):
                return match.group(0)
            resolved = (path.parent / relative).resolve()
            if resolved.is_relative_to(root) and resolved.is_file():
                return match.group(0)
            replaced += 1
            return label + ' [référence interne]'
        text = re.sub(r'(?<!!)\[([^\]]+)\]\(([^)]+)\)', link, path.read_text(encoding='utf-8'))
        if replaced:
            text = ('> Version de distribution : les références internes non livrées sont indiquées en texte. '
                    'Les preuves et plans détaillés restent dans le dépôt de développement ; aucun historique privé n’est fourni.\n\n' + text)
            path.write_text(text, encoding='utf-8')


def public_source_files(root, commit):
    """Exact, reviewed paths only. Adding a file to a directory never exports it."""
    raw = subprocess.check_output(['git', 'show', commit + ':packaging/v03-public-files.json'], cwd=root)
    names = json.loads(raw)
    if not isinstance(names, list) or len(names) != len(set(names)):
        raise ValueError('Invalid public source allowlist')
    for name in names:
        path = __import__('pathlib').PurePosixPath(name)
        if path.is_absolute() or '..' in path.parts or str(path) != name or '\\' in name:
            raise ValueError('Unsafe public source path: ' + name)
    return names
