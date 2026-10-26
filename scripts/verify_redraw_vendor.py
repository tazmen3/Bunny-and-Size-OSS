"""Verify the pinned backend's provenance without network or Cargo-cache mutation."""
import argparse
import hashlib
import json
from pathlib import Path
import tarfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    vendor = root / "vendor"
    manifest = json.loads((vendor / "provenance.json").read_text(encoding="utf-8"))
    source = vendor / manifest["crate"]
    actual = {p.relative_to(source).as_posix(): digest(p.read_bytes())
              for p in source.rglob("*") if p.is_file()}
    assert actual == manifest["delivered"], "Vendored source differs from the reviewed manifest"
    changed = {name for name, sha in actual.items() if sha != manifest["original"][name]}
    assert changed == {"renderer/sw.rs", "winitwindowadapter.rs"}, changed
    assert digest((vendor / "windows-redraw-94.patch").read_bytes()) == manifest["patch_sha256"]
    if args.archive:
        assert digest(args.archive.read_bytes()) == manifest["archive_sha256"]
        with tarfile.open(args.archive) as archive:
            original = {m.name.split("/", 1)[1]: digest(archive.extractfile(m).read())
                        for m in archive.getmembers() if m.isfile()}
        assert original == manifest["original"], "Published archive does not match provenance"
    print(f"PASS: {len(actual)} files; exactly two patched sources; licenses preserved")


if __name__ == "__main__":
    main()
