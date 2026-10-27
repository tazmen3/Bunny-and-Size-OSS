from pathlib import Path
import json
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import v04_distribution as d
from package_legal import write_manifest
from windows_signing import EXECUTABLES, verify_directory


class Distribution(unittest.TestCase):
    def info(self):
        bins = {n: dict(sha256=str(i) * 64, bytes=30) for i, n in enumerate(EXECUTABLES)}
        return dict(version='0.4.0-rc.1', source_commit='a' * 40,
                    source_archive=dict(sha256='b' * 64), binaries=bins,
                    built_from_commit='c' * 40, build_inputs_sha256='d' * 64,
                    build=dict(source_commit='c' * 40, version='0.4.0-rc.1', binaries=bins))

    def approval(self, info):
        return dict(schema=2, approved=True, owner='tazmen3', mode='public-unsigned',
                    scope='local-package-preparation', distribution_approved=False,
                    version=info['version'], source_commit=info['source_commit'],
                    source_archive_sha256=info['source_archive']['sha256'],
                    built_from_commit=info['built_from_commit'], build_inputs_sha256=info['build_inputs_sha256'],
                    qualified_binaries={'windows-x86_64': {n: r['sha256'] for n, r in info['binaries'].items()},
                                        'linux-x86_64': {n[:-4]: 'e' * 64 for n in EXECUTABLES}},
                    date='2026-10-27', decision_reference='SYNTHETIC TEST ONLY, NO OWNER GO')

    def test_no_preparation_and_each_wrong_binding_fail(self):
        info = self.info()
        with self.assertRaisesRegex(ValueError, 'authorization required'):
            d.authorize(None, info)
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'test-only.json'
            good = self.approval(info)
            for key in good:
                path.write_bytes(d.encode(dict(good, **{key: None})))
                with self.subTest(key=key), self.assertRaises((ValueError, KeyError)):
                    d.authorize(path, info)
            path.write_bytes(d.encode(good))
            self.assertFalse(d.authorize(path, info)['distribution_approved'])
            info['binaries']['sortsize.exe']['sha256'] = 'f' * 64
            with self.assertRaises(ValueError):
                d.authorize(path, info)
            for schema in [1, 3, True]:
                path.write_bytes(d.encode(dict(good, schema=schema)))
                with self.assertRaises(ValueError):
                    d.authorize(path, self.info())

    def fixture(self, root, mode):
        folder = root / ('Bunny-and-Size-v0.4.0-rc.1-' + mode)
        folder.mkdir()
        info = self.info()
        for n in EXECUTABLES:
            raw = b'MZ synthetic, never executable'
            (folder / n).write_bytes(raw)
            info['binaries'][n].update(sha256=d.sha(raw), bytes=len(raw))
        info.update(windows_distribution=mode, audience='review-only', redistribution_ready=False)
        (folder / 'BUILDINFO.json').write_bytes(d.encode(info))
        write_manifest(folder)
        return folder, info

    def prepare(self, root):
        folder, info = self.fixture(root, 'public-unsigned')
        auth = root / 'SYNTHETIC.json'
        auth.write_bytes(d.encode(self.approval(info)))
        info.update(authorization=d.authorize(auth, info), audience=d.LABEL,
                    preparation_approved=True, distribution_approved=False, public_release=False,
                    packaging_validated=True, intended_audience='public-prerelease',
                    release_approval_required='external-final-assets', status='READY FOR OWNER RELEASE APPROVAL')
        (folder / 'BUILDINFO.json').write_bytes(d.encode(info))
        write_manifest(folder)
        return folder, info, auth

    def test_local_archive_cannot_promote_preparation_to_public_approval(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, info, auth = self.prepare(root)
            for key in [None, 'distribution_approved', 'public_release', 'redistribution_ready']:
                modified = dict(info)
                if key:
                    modified[key] = True
                (folder / 'BUILDINFO.json').write_bytes(d.encode(modified))
                write_manifest(folder)
                path = root / (str(key) + '.zip')
                d.archive(folder, path)
                with patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory'):
                    if key:
                        with self.assertRaisesRegex(ValueError, 'cannot approve distribution'):
                            d.verify(path, 'public-unsigned', auth)
                    else:
                        result = d.verify(path, 'public-unsigned', auth)
                        self.assertFalse(result['public_authorized'])
                        self.assertEqual(result['status'], 'READY FOR OWNER RELEASE APPROVAL')

    def test_modes_cannot_be_confused_and_default_remains_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for mode in ['internal-unsigned', 'unsigned-review']:
                folder, _ = self.fixture(root, mode)
                path = root / (mode + '.zip')
                d.archive(folder, path)
                with self.assertRaisesRegex(ValueError, 'mode mismatch'):
                    d.verify(path)
                with self.assertRaisesRegex(ValueError, 'mode mismatch'):
                    d.verify(path, 'public-unsigned')

    def test_unsigned_preparation_rechecks_native_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, _, auth = self.prepare(root)
            path = root / 'test.zip'
            d.archive(folder, path)
            with patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory') as native:
                self.assertFalse(d.verify(path, 'public-unsigned', auth)['public_authorized'])
                self.assertEqual(native.call_args.args[2], False)
                self.assertEqual(native.call_args.kwargs, dict(unsigned=True))
            with self.assertRaises(ValueError):
                d.verify(path, 'public-unsigned')
            with patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory', side_effect=ValueError('invalid signature')):
                with self.assertRaisesRegex(ValueError, 'invalid signature'):
                    d.verify(path, 'public-unsigned', auth)

    def test_signed_archive_keeps_mandatory_native_signature_verification(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, info = self.fixture(root, 'public-signed')
            info['redistribution_ready'] = True
            (folder / 'BUILDINFO.json').write_bytes(d.encode(info))
            write_manifest(folder)
            path = root / 'signed.zip'
            d.archive(folder, path)
            with patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory') as native:
                self.assertFalse(d.verify(path)['public_authorized'])
                self.assertIs(native.call_args.args[2], True)
                self.assertEqual(native.call_args.kwargs, dict(unsigned=False))
            with patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory', side_effect=ValueError('signature required')):
                with self.assertRaisesRegex(ValueError, 'signature required'):
                    d.verify(path)

    def test_archive_determinism_checksums_and_tampering(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, _ = self.fixture(root, 'unsigned-review')
            for ext in ['.zip', '.tar.gz']:
                a, b = root / ('a' + ext), root / ('b' + ext)
                d.archive(folder, a)
                d.archive(folder, b)
                self.assertEqual(a.read_bytes(), b.read_bytes())
                d.read_archive(a)
            path = root / 'a.zip'
            with zipfile.ZipFile(path, 'a') as z:
                z.writestr(folder.name + '/UNLISTED', b'tamper')
            with self.assertRaisesRegex(ValueError, 'membership'):
                d.read_archive(path)

    def test_path_and_secret_markers_in_utf8_and_utf16_fail(self):
        for name in ['../escape', '/absolute', 'C:/evil', 'A/../x', 'CON.txt', 'a\\x', 'trailing.', 'a//b']:
            with self.subTest(name=name), self.assertRaises(ValueError):
                d.safe(name)
        for raw in [b'/'.join([b'', b'home', b'alice', b'lab', b'file']), b'\\'.join([b'C:', b'Users', b'Alice', b'lab']), b'ghp_' + b'A' * 30]:
            for content in [raw, raw.decode().encode('utf-16le')]:
                with self.assertRaisesRegex(ValueError, 'credential marker'):
                    d.privacy('program.exe', content)

    def test_duplicate_manifests_and_case_collision_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, _ = self.fixture(root, 'unsigned-review')
            data = d.files(folder)
            rows = json.loads(data['FILES.json'])
            data['FILES.json'] = d.encode(rows + rows[:1])
            with self.assertRaisesRegex(ValueError, 'membership'):
                d.verify_files(data)
            path = root / 'case.zip'
            d.archive(folder, path)
            # Author the collision in ZIP metadata: NTFS cannot retain both
            # case variants as separate ordinary files.
            with zipfile.ZipFile(path, 'a') as z:
                z.writestr(folder.name + '/SORTSIZE.EXE', b'MZ bad')
            with self.assertRaisesRegex(ValueError, 'Duplicate'):
                d.read_archive(path)

    def test_reusing_builds_requires_every_compiler_input_but_allows_document_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            a, b = Path(tmp) / 'a', Path(tmp) / 'b'
            a.mkdir(); b.mkdir()
            rows = [dict(path=n, sha256='a' * 64) for n in ['Cargo.lock', 'apps/main.rs', 'vendor/lib.rs', 'cargo-vendor/lib/src.rs', 'assets/icon.png', '.cargo/config.toml', 'packaging/windows/version-resource.rs', 'scripts/v04_distribution.py', 'docs/guide.md']]
            (a / 'FILES.json').write_bytes(d.encode(rows))
            for i, row in enumerate(rows):
                changed = [dict(r) for r in rows]
                changed[i]['sha256'] = 'b' * 64
                (b / 'FILES.json').write_bytes(d.encode(changed))
                self.assertEqual(d.build_inputs(a) == d.build_inputs(b), row['path'].startswith(('scripts/', 'docs/')))

    def test_archive_admission_budget_and_nonempty_pe_security_directory_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder, _ = self.fixture(root, 'unsigned-review')
            path = root / 'candidate.zip'
            d.archive(folder, path)
            with patch('v04_distribution.MAX_ARCHIVE_BYTES', 10):
                with self.assertRaisesRegex(ValueError, 'admission budget'):
                    d.read_archive(path)
            for n in EXECUTABLES:
                raw = bytearray(513);raw[:2] = b'MZ'
                struct.pack_into('<I', raw, 0x3c, 0x80)
                struct.pack_into('<H', raw, 0x98, 0x20b)
                struct.pack_into('<II', raw, 0x98 + 112 + 4 * 8, 512, 8)
                (root / n).write_bytes(raw)
            path.unlink()
            import shutil
            shutil.rmtree(folder)
            with self.assertRaisesRegex(ValueError, 'empty security directory'):
                verify_directory(root, '0.4.0-rc.1', False, unsigned=True)

    def test_selection_contains_current_v04_build_inputs_and_no_private_paths(self):
        names = json.loads((d.ROOT / 'packaging/v04-public-files.json').read_text())
        for n in ['crates/sortsize-core/src/duplicates.rs', 'crates/sortsize-core/src/analysis.rs',
                  'apps/sortsize-cli/src/analysis_output.rs', 'spikes/slint/lang/it/LC_MESSAGES/sortsize-slint-spike.po',
                  'spikes/slint/lang/nl/LC_MESSAGES/sortsize-slint-spike.po']:
            self.assertIn(n, names)
        for n in names:
            d.safe(n)
            d.privacy(n, (d.ROOT / n).read_bytes())



class ReleaseGate(unittest.TestCase):
    info = Distribution.info
    approval = Distribution.approval
    # Uses tiny synthetic archives, never a real release decision or executable.
    def delivery_fixture(self, root):
        delivery = root / 'delivery'
        delivery.mkdir()
        version = '0.4.0-rc.1'
        prefix = 'Bunny-and-Size-v' + version
        source = root / (prefix + '-source')
        source.mkdir()
        selection = dict(version=version, source_commit='a' * 40, audience='public-prerelease',
                         intended_distribution='public-unsigned', release_approval_required='external-final-assets',
                         distribution_approved=False, public_release=False, selection=[])
        (source / 'SOURCE_SELECTION.json').write_bytes(d.encode(selection))
        (source / 'Cargo.toml').write_bytes(b'synthetic compiler input')
        write_manifest(source)
        source_record = d.archive(source, delivery / (source.name + '.tar.gz'))
        inputs = d.build_inputs(source)
        input_sha = d.sha(json.dumps(inputs, sort_keys=True).encode())
        auth = delivery / 'OWNER_PREPARATION_AUTHORIZATION.json'
        infos = {}
        for platform in ['windows-x86_64', 'linux-x86_64']:
            info = self.info()
            if platform.startswith('linux'):
                info['binaries'] = {n[:-4]: dict(sha256=d.sha(b'ELF fixture'), bytes=11) for n in EXECUTABLES}
            else:
                info['binaries'] = {n: dict(sha256=d.sha(b'MZ fixture'), bytes=10) for n in EXECUTABLES}
            info['build'] = dict(source_commit=info['built_from_commit'], version=version, binaries=info['binaries'])
            info['source_archive'] = dict(filename=source_record['filename'], sha256=source_record['sha256'])
            info['build_inputs_sha256'] = input_sha
            info.update(windows_distribution='public-unsigned', audience=d.LABEL, redistribution_ready=False,
                        preparation_approved=True, distribution_approved=False, public_release=False,
                        packaging_validated=True, intended_audience='public-prerelease',
                        release_approval_required='external-final-assets', status='READY FOR OWNER RELEASE APPROVAL')
            infos[platform] = info
        prep = self.approval(infos['windows-x86_64'])
        prep['qualified_binaries']['linux-x86_64'] = {n: v['sha256'] for n, v in infos['linux-x86_64']['binaries'].items()}
        auth.write_bytes(d.encode(prep))
        for platform, info in infos.items():
            info['authorization'] = d.authorize(auth, info)
            folder = root / (prefix + '-public-unsigned-' + platform)
            folder.mkdir()
            for n in info['binaries']:
                (folder / n).write_bytes(b'MZ fixture' if platform.startswith('windows') else b'ELF fixture')
            (folder / 'BUILDINFO.json').write_bytes(d.encode(info))
            write_manifest(folder)
            d.archive(folder, delivery / (folder.name + ('.zip' if platform.startswith('windows') else '.tar.gz')))
        provenance = dict(version=version, mode='public-unsigned', source_commit='a' * 40,
                          built_from_commit='c' * 40, build_inputs_sha256=input_sha,
                          qualified_binaries={p: i['binaries'] for p, i in infos.items()},
                          source_selection_sha256=d.sha((source / 'SOURCE_SELECTION.json').read_bytes()),
                          compiler_input_count=len(inputs), distribution_approved=False,
                          status='READY FOR OWNER RELEASE APPROVAL')
        (delivery / 'PROVENANCE.json').write_bytes(d.encode(provenance))
        (delivery / 'RELEASE_NOTES.md').write_bytes(b'Prerelease synthetic fixture')
        self.sums(delivery)
        return delivery

    def sums(self, delivery):
        (delivery / 'SHA256SUMS').write_text(''.join(d.sha(p.read_bytes()) + '  ' + p.name + '\n'
            for p in sorted(delivery.iterdir()) if p.name != 'SHA256SUMS'), encoding='utf-8')

    def test_final_external_contract_and_all_negative_bindings(self):
        with tempfile.TemporaryDirectory() as tmp, patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory'):
            root = Path(tmp)
            delivery = self.delivery_fixture(root)
            verify = lambda **kw: d.verify_delivery(delivery, '0.4.0-rc.1', **kw)
            result = verify()
            self.assertEqual(result['status'], 'READY FOR OWNER RELEASE APPROVAL')
            self.assertFalse(result['public_authorized'])
            with self.assertRaisesRegex(ValueError, 'publication blocked'):
                verify(require_publication=True)
            with self.assertRaises(ValueError):
                verify(release_approval=delivery / 'OWNER_PREPARATION_AUTHORIZATION.json')
            external = root / 'SYNTHETIC-FINAL.json'
            context = result['context']
            good = dict(context, schema=3, approved=True, owner='tazmen3', scope='final-asset-publication',
                        date='2026-10-27', decision_reference='SYNTHETIC TEST ONLY', synthetic=True)
            external.write_bytes(d.encode(good))
            with self.assertRaisesRegex(ValueError, 'Synthetic'):
                verify(release_approval=external)
            simulated = d.verify_delivery(delivery, '0.4.0-rc.1', release_approval=external, require_publication=True, allow_synthetic=True)
            self.assertTrue(simulated['authorization_validated'])
            self.assertFalse(simulated['public_authorized'])
            self.assertEqual(simulated['status'], 'SYNTHETIC TEST ONLY')
            for key in good:
                bad = dict(good, **{key: None})
                external.write_bytes(d.encode(bad))
                with self.subTest(key=key), self.assertRaises(ValueError):
                    d.release_authorization(external, context, allow_synthetic=True)
            for key, value in [('mode', 'public-signed'), ('version', '0.4.0-rc.2'),
                               ('source_commit', 'f' * 40), ('built_from_commit', 'f' * 40),
                               ('assets', context['assets'][:-1])]:
                external.write_bytes(d.encode(dict(good, **{key: value})))
                with self.subTest(key=key), self.assertRaises(ValueError):
                    d.release_authorization(external, context, allow_synthetic=True)
            external.write_bytes(d.encode(good))
            # Recompressed or otherwise modified archives cannot reuse a decision.
            zipped = next(delivery.glob('*.zip'))
            original = zipped.read_bytes()
            zipped.write_bytes(original + b'changed after authorization')
            self.sums(delivery)
            changed = verify()['context']
            with self.assertRaisesRegex(ValueError, 'exact assets'):
                d.release_authorization(external, changed, allow_synthetic=True)
            zipped.write_bytes(original)
            self.sums(delivery)
            # Even an internally consistent new delivery cannot reuse the final decision.
            (delivery / 'RELEASE_NOTES.md').write_bytes(b'changed after approval')
            self.sums(delivery)
            changed = verify()['context']
            with self.assertRaisesRegex(ValueError, 'exact assets'):
                d.release_authorization(external, changed, allow_synthetic=True)
            # A preparation record, even external, never substitutes for schema 3.
            external.write_bytes((delivery / 'OWNER_PREPARATION_AUTHORIZATION.json').read_bytes())
            with self.assertRaises(ValueError):
                verify(release_approval=external)

    def test_source_executable_and_version_tampering(self):
        for target in ['source', 'binary', 'mode', 'version', 'provenance']:
            with self.subTest(target=target), tempfile.TemporaryDirectory() as tmp, patch('v04_distribution.verify_disclosures'), patch('v04_distribution.verify_directory'):
                root = Path(tmp)
                delivery = self.delivery_fixture(root)
                if target == 'provenance':
                    path = delivery / 'PROVENANCE.json'
                    value = json.loads(path.read_bytes())
                    value['built_from_commit'] = 'f' * 40
                    path.write_bytes(d.encode(value))
                else:
                    folder = next(p for p in root.iterdir() if p.is_dir() and ('source' in p.name if target == 'source' else 'windows' in p.name))
                    if target == 'source':
                        (folder / 'Cargo.toml').write_bytes(b'altered compiler input')
                    elif target == 'binary':
                        (folder / 'sortsize.exe').write_bytes(b'MZ altered')
                    else:
                        path = folder / 'BUILDINFO.json'
                        info = json.loads(path.read_bytes())
                        info['windows_distribution' if target == 'mode' else 'version'] = 'public-signed' if target == 'mode' else '0.4.0-rc.2'
                        path.write_bytes(d.encode(info))
                    write_manifest(folder)
                    path = delivery / (folder.name + ('.tar.gz' if target == 'source' else '.zip'))
                    path.unlink()
                    d.archive(folder, path)
                self.sums(delivery)
                with self.assertRaises(ValueError):
                    d.verify_delivery(delivery, '0.4.0-rc.1')


if __name__ == '__main__':
    unittest.main()
