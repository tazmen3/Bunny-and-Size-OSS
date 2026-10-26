#!/usr/bin/env python3
"""Verify the owner's explicit artwork decision; never infer it from the code GPL."""
import argparse
import hashlib
import json
from pathlib import Path


def check_artwork_rights(root, internal_review=False):
    path = root / 'assets/branding/PROVENANCE.json'
    info = json.loads(path.read_text(encoding='utf-8'))
    for name, digest in info['files'].items():
        relative = Path(name)
        if relative.is_absolute() or '..' in relative.parts or not name.startswith('assets/branding/'):
            raise ValueError('Unsafe artwork path')
        if hashlib.sha256((root / relative).read_bytes()).hexdigest() != digest:
            raise ValueError('Artwork hash mismatch: ' + name)
    approval = info.get('owner_approval')
    if any('original' in name.lower() or 'portrait' in name.lower() for name in info['files']):
        raise ValueError('Private artwork must not be licensed or exported')
    approved = (info.get('license_status') == 'approved' and info.get('license') == 'CC-BY-SA-4.0'
                and isinstance(approval, dict) and approval.get('owner') == info['author']
                and bool(approval.get('date')) and bool(approval.get('evidence'))
                and approval.get('files') == info['files'])
    if approved:
        license_path = root / 'assets/branding/CC-BY-SA-4.0.txt'
        if not license_path.is_file() or hashlib.sha256(license_path.read_bytes()).hexdigest() != '28a9529c7d0bb4dc51f4bf5c116a3d16ef247a052f7591466768ddf563fd1cf5':
            raise ValueError('Missing or altered CC BY-SA 4.0 license text')
    if not approved and not internal_review:
        raise ValueError('Artwork license requires explicit owner approval; no redistribution candidate allowed')
    return dict(status='approved' if approved else 'pending-owner-approval',
                license=info.get('license'), internal_review=internal_review,
                redistribution_ready=bool(approved and not internal_review))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--internal-review', action='store_true', help='Private test only; grants no distribution rights')
    args = parser.parse_args()
    print(json.dumps(check_artwork_rights(Path(__file__).resolve().parents[1], args.internal_review), indent=2))


if __name__ == '__main__':
    main()
