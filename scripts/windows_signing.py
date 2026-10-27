"""Windows release gate. No signing keys, network uploads or trust-store changes."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[1]
POLICY = ROOT / 'packaging/windows/signing-policy.json'
EXECUTABLES = ('bunny-and-size.exe', 'bunny-and-size-cli.exe', 'sortsize.exe')


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def load_policy(public):
    policy = json.loads(POLICY.read_text(encoding='utf-8'))
    if policy.get('schema') != 1 or policy.get('public_signatures_required') is not True:
        raise ValueError('Invalid mandatory signing policy')
    if public and (not policy.get('publisher_subject') or not
                   re.fullmatch('[0-9a-f]{64}', policy.get('certificate_sha256') or '')):
        raise ValueError('Public signing identity is not configured; release blocked')
    return policy


def inventory(directory):
    """Reject extra/nested PE files, symlinks, missing or renamed executables."""
    found = set()
    for path in directory.rglob('*'):
        if path.is_symlink():
            raise ValueError('Links are not allowed in Windows packages')
        if not path.is_file():
            continue
        with path.open('rb') as stream:
            magic = stream.read(2)
        executable_suffixes = ('.exe', '.dll', '.sys', '.msi', '.msix', '.scr', '.com',
                               '.bat', '.cmd', '.ps1', '.psm1', '.vbs', '.js', '.wsf',
                               '.cpl', '.ocx', '.efi', '.drv', '.hta', '.lnk', '.url')
        if magic == b'MZ' or path.suffix.lower() in executable_suffixes:
            relative = path.relative_to(directory).as_posix()
            if relative not in EXECUTABLES or magic != b'MZ':
                raise ValueError('Unexpected executable: ' + relative)
            found.add(relative)
    if found != set(EXECUTABLES):
        raise ValueError('Missing Windows executable')
    return {name: sha((directory / name).read_bytes()) for name in EXECUTABLES}


def verify_directory(directory, version, public=True, unsigned=False):
    if public and unsigned:
        raise ValueError('Conflicting signature modes')
    directory = Path(directory).resolve()
    load_policy(public)
    before = inventory(directory)
    if unsigned:
        for name in EXECUTABLES:
            raw = (directory / name).read_bytes()
            optional = struct.unpack_from('<I', raw, 0x3c)[0] + 24
            if struct.unpack_from('<H', raw, optional)[0] != 0x20b or any(struct.unpack_from('<II', raw, optional + 112 + 4 * 8)):
                raise ValueError('Unsigned PE must have an empty security directory')
    if os.name != 'nt':
        raise ValueError('Native Windows Authenticode verification is required')
    # -EncodedCommand avoids shell interpolation and leaves ExecutionPolicy intact.
    def quote(value):
        return "'" + str(value).replace("'", "''") + "'"
    script = ROOT / 'scripts/verify-windows-signatures.ps1'
    command = ('[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false);'
               + '& ([scriptblock]::Create([IO.File]::ReadAllText(' + quote(script) + ')))'
               + ' -Directory ' + quote(directory) + ' -Version ' + quote(version)
               + ' -PolicyFile ' + quote(POLICY) + (' -RequireSignature' if public else '')
               + (' -RequireUnsigned' if unsigned else ''))
    encoded = base64.b64encode(command.encode('utf-16le')).decode('ascii')
    result = subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-EncodedCommand', encoded],
                            capture_output=True, check=True, timeout=180)
    report = json.loads(result.stdout.decode('utf-8-sig'))
    rows = report['files']
    if unsigned and (public or any(row['status'] != 'NotSigned' or row['publisher_subject']
                                  or row['certificate_sha256'] for row in rows)):
        raise ValueError('Unsigned distribution requires three NotSigned executables')
    if (report['public_verified'] is not public or len(rows) != len(EXECUTABLES)
            or {row['name']: row['sha256'] for row in rows} != before
            or inventory(directory) != before):
        raise ValueError('Native verification/inventory mismatch')
    return report


def signing_delta(original, signed):
    """Only checksum/security-directory changes and an appended WIN_CERTIFICATE.

    This is a provenance check, NOT a cryptographic signature verification.
    Native Authenticode + SignTool verification is independently mandatory.
    Existing signed inputs are deliberately rejected (no silent replacement).
    """
    def offsets(raw):
        if raw[:2] != b'MZ':
            raise ValueError('Not a PE')
        offset = struct.unpack_from('<I', raw, 0x3c)[0]
        if raw[offset:offset + 4] != b'PE\0\0':
            raise ValueError('Invalid PE header')
        optional = offset + 24
        if struct.unpack_from('<H', raw, optional)[0] != 0x20b:
            raise ValueError('PE32+ required')
        if struct.unpack_from('<I', raw, optional + 108)[0] < 5:
            raise ValueError('Missing security directory')
        security = optional + 112 + 4 * 8
        return optional + 64, security, struct.unpack_from('<II', raw, security)
    checksum, security, (old_offset, old_size) = offsets(original)
    new_checksum, new_security, (cert_offset, cert_size) = offsets(signed)
    if (old_offset or old_size or (checksum, security) != (new_checksum, new_security)
            or cert_offset != (len(original) + 7) // 8 * 8 or cert_size < 8
            or cert_offset + cert_size != len(signed)):
        raise ValueError('Unexpected signature layout')
    if any(signed[len(original):cert_offset]):
        raise ValueError('Nonzero signature padding')
    old, new = bytearray(original), bytearray(signed[:len(original)])
    for start, size in ((checksum, 4), (security, 8)):
        old[start:start + size] = new[start:start + size] = bytes(size)
    if old != new:
        raise ValueError('Signed file changed product bytes')
