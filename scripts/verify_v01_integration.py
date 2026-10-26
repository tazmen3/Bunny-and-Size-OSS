#!/usr/bin/env python3
"""Contrôles hors ligne de l’intégration et, sur demande, des quatre archives finales."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
import tomllib
import zipfile
from package_legal import legal_inputs, product_identity

ROOT = Path(__file__).resolve().parents[1]
REFERENCES = {
    'licences-v0.1': 'c4affb3f4aadd563714161ea6534cef888502d5e',
    'licences-v0.1-reconciliation': 'c4affb3f4aadd563714161ea6534cef888502d5e',
    'issue-47-windows': 'c05925086ebd2df1e35cfc57fa5cd1418a0d415f',
    'issue-47-linux': 'c0aed605f6f83b8a307201f48c7bb2a2bb098e0c',
}


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify_release(paths, commit, platforms=('linux', 'windows')):
    """Pas d’extraction ; lecture des contenus et contrôle de leur provenance commune."""
    targets = {'linux': 'x86_64-unknown-linux-gnu', 'windows': 'x86_64-pc-windows-msvc'}
    require(len(paths) == 2 * len(platforms), 'Fournir un couple binaire/source complet par plateforme demandée.')
    expected_tree = product_identity(ROOT, commit)
    results = []
    for path in paths:
        archive = zipfile.ZipFile(path) if path.suffix == '.zip' else tarfile.open(path, 'r:gz')
        with archive:
            if isinstance(archive, zipfile.ZipFile):
                members = [m for m in archive.infolist() if not m.is_dir()]
                read = lambda m: archive.read(m)
                names = [m.filename for m in members]
            else:
                members = [m for m in archive.getmembers() if m.isfile()]
                require(all(m.isfile() or m.isdir() for m in archive.getmembers()), 'Liens inattendus dans tar')
                read = lambda m: archive.extractfile(m).read()
                names = [m.name for m in members]
            require(len(names) == len(set(names)), 'Entrée dupliquée : ' + str(path))
            roots = {PurePosixPath(n).parts[0] for n in names}
            require(len(roots) == 1, 'Racine unique requise')
            mapping = {}
            for name, member in zip(names, members):
                safe = PurePosixPath(name)
                require(not safe.is_absolute() and '..' not in safe.parts, 'Chemin non sûr')
                mapping['/'.join(safe.parts[1:])] = member
            info = json.loads(read(mapping['BUILDINFO.json']))
            require(info['source_commit'] == commit, 'Commit candidat ancien ou différent : ' + str(path))
            require(info.get('product_tree_sha256') == expected_tree, 'Identité produit absente/différente')
            require(read(mapping['LICENSE']) == git('show', commit + ':LICENSE'), 'GPL différente de Git')
            for required in ('AUTHORS.txt', 'THIRD_PARTY_NOTICES.txt', 'README.md'):
                require(bool(read(mapping[required])), 'Notice absente/vide : ' + required)
            rows = json.loads(read(mapping['FILES.json']))
            listed = {r['path'] for r in rows}
            require(len(rows) == len(listed), 'Manifest avec doublon')
            require(listed == set(mapping) - {'FILES.json', 'SHA256SUMS'}, 'Manifest incomplet')
            for row in rows:
                require(len(read(mapping[row['path']])) == row['bytes'], 'Taille de fichier incorrecte')
                require(sha(read(mapping[row['path']])) == row['sha256'], 'Hash de fichier incorrect')
            sums = read(mapping['SHA256SUMS']).decode().splitlines()
            require(len(sums) == len(mapping) - 1, 'SHA256SUMS incomplet')
            sum_names = set()
            for line in sums:
                digest, name = line.split('  ', 1)
                require(name not in sum_names, 'Empreinte dupliquée')
                sum_names.add(name)
                require(sha(read(mapping[name])) == digest, 'Empreinte incorrecte : ' + name)
            require(sum_names == set(mapping) - {'SHA256SUMS'}, 'Couverture SHA256SUMS incorrecte')
            source = 'SOURCE_COMPONENTS.json' in mapping
            if source:
                for relative, digest in info['lock_git_sha256'].items():
                    require(sha(read(mapping[relative])) == digest, 'Lockfile source différent')
                for relative in git('ls-tree', '-r', '--name-only', commit, '--', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'apps', 'crates', 'spikes/slint', 'packaging', 'scripts/package_legal.py', 'scripts/package_linux.py', 'scripts/package-linux.sh', 'scripts/package_windows.py', 'scripts/package-windows.ps1', 'scripts/verify_v01_integration.py', 'docs/ADR/0014-avis-interactifs-gplv3.md').decode().splitlines():
                    require(read(mapping[relative]) == git('show', commit + ':' + relative), 'Source produit différente : ' + relative)
                locked = {}
                for relative in ('Cargo.lock', 'spikes/slint/Cargo.lock'):
                    locked.update({(p['name'], p['version']): p.get('checksum') for p in tomllib.loads(read(mapping[relative]).decode())['package']})
                components = json.loads(read(mapping['SOURCE_COMPONENTS.json']))
                require(len(components) == info['source_registry_archives'], 'Nombre de sources Cargo incorrect')
                for component in components:
                    name, version = component['name'], component['version']
                    digest = sha(read(mapping[f'vendor-registry/{name}-{version}.crate']))
                    require(digest == locked[name, version] == component['archive_sha256'], 'Source Cargo incorrecte')
                    for ref in component['texts']:
                        require(sha(read(mapping[ref['path']])) == ref['sha256'], 'Texte légal différent')
            else:
                for name, binary in info['binaries'].items():
                    require(sha(read(mapping[name])) == binary['sha256'], 'Exécutable différent')
            results.append({'path': str(path), 'sha256': sha(path.read_bytes()), 'source': source,
                            'target': info['target'], 'info': info})
    require(len({json.dumps(r['info']['lock_git_sha256'], sort_keys=True) for r in results}) == 1, 'Lockfiles différents entre plateformes')
    require({r['target'] for r in results} == {targets[p] for p in platforms}, 'Plateformes inattendues')
    for target in (targets[p] for p in platforms):
        pair = [r for r in results if r['target'] == target]
        require(len(pair) == 2 and {r['source'] for r in pair} == {False, True}, 'Couple cible incomplet')
        source = next(r for r in pair if r['source'])
        binary = next(r for r in pair if not r['source'])
        require(binary['info']['source_archive']['sha256'] == source['sha256'], 'Mauvais source correspondant')
        require(binary['info']['binaries'] == source['info']['binaries'], 'Exécutables/provenance désaccordés')
        require(binary['info']['lock_git_sha256'] == source['info']['lock_git_sha256'], 'Lockfiles désaccordés')
    return [{k: v for k, v in r.items() if k != 'info'} for r in results]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--platform', choices=('all', 'linux', 'windows'), default='all', help='all exige les quatre archives ; une plateforme exige son couple uniquement')
    parser.add_argument('--commit', default='HEAD', help='Commit source attendu, à figer pour la livraison')
    parser.add_argument('--release-archive', action='append', type=Path, default=[])
    args = parser.parse_args()
    commit = git('rev-parse', args.commit).decode().strip()
    historical = {}
    for folder, ref in REFERENCES.items():
        relative = 'docs/validation/' + folder
        require(not git('diff', '--name-only', ref, 'HEAD', '--', relative).strip(), 'Preuve historique modifiée : ' + folder)
        directory = ROOT / relative
        count = 0
        for manifest in directory.rglob('SHA256SUMS'):
            for line in manifest.read_text(encoding='utf-8').splitlines():
                digest, name = line.split('  ', 1)
                data = (manifest.parent / name).read_bytes()
                require(sha(data) == digest, 'Preuve incorrecte : ' + str(manifest.parent / name))
                count += 1
        historical[folder] = count
    locks = {}
    union = {}
    for relative in ('Cargo.lock', 'spikes/slint/Cargo.lock'):
        data = (ROOT / relative).read_bytes()
        require(data.replace(b'\r\n', b'\n') == git('show', 'c0aed60:' + relative), 'Lockfile modifié')
        locks[relative] = sha(git('show', commit + ':' + relative))
        union.update({(p['name'], p['version']): p.get('checksum') for p in tomllib.loads(data.decode())['package']})
    legal = legal_inputs(ROOT)
    targeted = {}
    reserve_dir = ROOT / 'docs/validation/v01-integration'
    reserves = json.loads((reserve_dir / 'legal-reserves.json').read_text(encoding='utf-8'))
    cache = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')) / 'registry/cache'
    for name in ('drm-fourcc', 'skia-bindings'):
        info = reserves[name]
        key = (name, info['version'])
        archive_path = next(cache.glob('*/' + name + '-' + info['version'] + '.crate'))
        require(sha(archive_path.read_bytes()) == union[key] == info['crate_sha256'], 'Archive ciblée incorrecte')
        expected = json.loads((reserve_dir / (name + '-' + info['version'] + '-files.json')).read_text())
        with tarfile.open(archive_path) as archive:
            actual = {m.name.split('/', 1)[1]: sha(archive.extractfile(m).read()) for m in archive if m.isfile()}
            require(actual == expected, 'Contenu de l’archive ciblée différent')
        targeted[name] = {'checksum': info['crate_sha256'], 'files_verified': len(actual)}
    tree = product_identity(ROOT, commit)
    identities = {ref: product_identity(ROOT, ref) for ref in ('355e7ee', 'e95ec9d', 'c0aed60')}
    product_changed = any(v != tree for v in identities.values())
    subprocess.run([sys.executable, 'scripts/render_notices_v01.py', '--check'], cwd=ROOT, check=True, stdout=subprocess.PIPE)
    result = {'commit_checked': commit, 'historical_manifests_verified': historical,
              'lock_git_sha256': locks, 'product_tree_sha256': tree, 'historical_product_identities': identities,
              'legal_inputs_verified': len(legal), 'targeted_archives': targeted, 'notice_render': 'passed',
              'product_changed': product_changed, 'validation_evidence': 'Consulter le rapport externe du commit figé ; ce contrôle ne lance pas les tests Rust ni la recette native.',
              'distribution_compliance': 'not yet demonstrated; see docs/V01_INTEGRATION.md'}
    if args.release_archive:
        result['release_archives_verified'] = verify_release(args.release_archive, commit, ('linux', 'windows') if args.platform == 'all' else (args.platform,))
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
