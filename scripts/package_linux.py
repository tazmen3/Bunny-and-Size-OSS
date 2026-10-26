#!/usr/bin/env python3
"""Archives Linux et source correspondant candidats ; aucune publication ni suppression.

Python >= 3.11 standard. Le corpus #50 est une entrée externe en lecture seule.
Les archives Cargo du registre local sont inchangées ; seul l'index nécessaire
à la résolution des lockfiles est exporté du cache public (aucun identifiant).
"""
from package_legal import check_audit, legal_inputs, legal_texts, product_identity, write_manifest, product_version, check_registry_lock, local_backend_notice, public_documentation, public_source_files

import argparse
from verify_artwork_rights import check_artwork_rights
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import struct
import subprocess
import tarfile
import time
import tomllib
import gzip
import io

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"
AUDIT_COMMIT = "c4affb3f4aadd563714161ea6534cef888502d5e"
VERSION = product_version(ROOT)
NAME = f"Bunny-and-Size-v{VERSION}-linux-x86_64"
SOURCE_NAME = f"Bunny-and-Size-v{VERSION}-source-linux-x86_64"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def run(*args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd)


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, str):
        data = data.encode("utf-8")
    path.write_bytes(data)


def write_json(path, value):
    save(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def copy_verified(source, destination, expected):
    data = source.read_bytes()
    if sha(data) != expected:
        raise ValueError(f"Hash incorrect : {source}")
    save(destination, data)


def elf(path):
    data = path.read_bytes()
    if data[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", data, 18)[0] != 62:
        raise ValueError(f"ELF Linux x86_64 attendu : {path}")
    return {"sha256": sha(data), "bytes": len(data), "machine": "x86_64", "format": "ELF64"}


def index_path(name):
    if len(name) == 1:
        return name
    if len(name) == 2:
        return "2/" + name
    if len(name) == 3:
        return "3/" + name[0] + "/" + name
    return name[:2] + "/" + name[2:4] + "/" + name


def make_tar(directory, destination, epoch):
    # Aucun timestamp gzip courant, ordre/uid/gid/modes fixes, aucune entrée cache.
    with destination.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for path in sorted(p for p in directory.rglob("*") if p.is_file()):
                    info = tarfile.TarInfo(directory.name + "/" + path.relative_to(directory).as_posix())
                    data = path.read_bytes()
                    info.size = len(data)
                    info.mtime = epoch
                    info.mode = 0o755 if path.name in ("bunny-and-size", "bunny-and-size-cli", "sortsize", "rebuild-linux.sh") else 0o644
                    archive.addfile(info, io.BytesIO(data))



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--audit-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--internal-review", action="store_true", help="Private recipe only; no artwork distribution rights granted")
    args = parser.parse_args()
    artwork_rights = check_artwork_rights(ROOT, args.internal_review)
    audit = args.audit_root.resolve()
    check_audit(ROOT, audit, AUDIT_COMMIT)
    commit = run("git", "rev-parse", "HEAD").decode().strip()
    # Docs/outils peuvent être en préparation ; tout code produit doit être commité.
    if run("git", "diff", "HEAD", "--", "Cargo.toml", "Cargo.lock", "apps", "crates", "spikes").strip():
        raise ValueError("Les sources produit doivent correspondre exactement à HEAD.")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = output / NAME
    source = output / SOURCE_NAME
    binary.mkdir()
    source.mkdir()
    release = args.target_dir.resolve() / TARGET / "release"
    binaries = {}
    for original, packaged in [("bunny-and-size", "bunny-and-size"), ("bunny-and-size-cli", "bunny-and-size-cli"), ("sortsize", "sortsize")]:
        binaries[packaged] = elf(release / original)
        save(binary / packaged, (release / original).read_bytes())
        (binary / packaged).chmod(0o755)
    epoch = int(run("git", "show", "-s", "--format=%ct", commit))
    lock_hashes = {}
    locked = {}
    for relative in ["Cargo.lock", "spikes/slint/Cargo.lock"]:
        data = run("git", "show", commit + ":" + relative)
        reference = run("git", "show", AUDIT_COMMIT + ":" + relative, cwd=audit)
        check_registry_lock(data, reference)
        lock_hashes[relative] = sha(data)
        for package in tomllib.loads(data.decode())["package"]:
            if package.get("source"):
                assert package["source"].startswith("registry+")
                locked[package["name"], package["version"]] = package["checksum"]
    old = audit / "docs/validation/licences-v0.1"
    proof_hashes = dict((name, digest) for digest, name in
                        (line.split("  ", 1) for line in (old / "SHA256SUMS").read_text(encoding="utf-8").splitlines()))
    legal = {(e["name"], e["version"]): e for e in json.loads((old / "legal-index.json").read_text(encoding="utf-8"))}
    decisions = {(e["name"], e["version"]): e for e in json.loads((old / "license-decisions.json").read_text(encoding="utf-8"))}
    delta = json.loads((audit / "docs/validation/licences-v0.1-reconciliation/delta.json").read_text(encoding="utf-8"))
    additions = {(e["name"], e["version"]): e for e in delta["packages"]}
    source_supplements = legal_inputs(ROOT)
    metadata = {}
    roles = {}
    for label, manifest, package in [("cli", "Cargo.toml", "sortsize-cli"), ("gui", "spikes/slint/Cargo.toml", "sortsize-slint-spike")]:
        data = json.loads(run("cargo", "metadata", "--locked", "--offline", "--manifest-path", manifest, "--filter-platform", TARGET, "--format-version", "1"))
        metadata.update({(e["name"], e["version"]): e for e in data["packages"] if e["source"]})
        for edges, role in [("normal,build", "production/build"), ("normal,no-proc-macro", "runtime")]:
            tree = run("cargo", "tree", "--locked", "--offline", "--manifest-path", manifest, "-p", package, "--target", TARGET, "-e", edges, "--prefix", "none", "--format", "{p}").decode()
            for key in re.findall(r"^([^\s]+) v([^\s]+)", tree, re.M):
                if key in locked:
                    roles.setdefault(key, set()).add(label + ":" + role)
    # Cargo local-registry exige aussi les archives de résolution optionnelle,
    # confirmé par une reconstruction --frozen depuis CARGO_HOME neuf.
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    caches = list((cargo_home / "registry/cache").iterdir())
    index_cache = next((cargo_home / "registry/index").glob("*/.cache"))
    registry = source / "vendor-registry"
    registry.mkdir()
    # L'index contient les résumés de toutes les versions verrouillées ; aucune
    # source inactive d'une autre plateforme n'est ajoutée pour ces résumés.
    for name in sorted({key[0] for key in locked}):
        rows = []
        for raw in (index_cache / index_path(name)).read_bytes().split(b"\0"):
            if raw.startswith(b"{"):
                entry = json.loads(raw)
                if (name, entry["vers"]) in locked:
                    assert entry["cksum"] == locked[name, entry["vers"]]
                    rows.append(raw)
        assert rows, name
        save(registry / "index" / index_path(name), b"\n".join(rows) + b"\n")
    component_rows = []
    binary_notices = []
    source_notices = []
    for key, package in sorted(metadata.items()):
        name, version = key
        archive = next(p / f"{name}-{version}.crate" for p in caches if (p / f"{name}-{version}.crate").exists())
        checksum = locked[key]
        copy_verified(archive, registry / archive.name, checksum)
        files = []
        selected = None
        if key in legal:
            assert legal[key]["crate_sha256_verified"] == checksum
            selected = decisions[key]["license_chosen"]
            for relative in decisions[key]["evidence_files"] + [decisions[key]["copyright_file"]]:
                original = old / relative
                assert sha(original.read_bytes()) == proof_hashes[relative]
                files.append(("licences/cargo/" + relative, original.read_bytes()))
                if relative.startswith("legal/") and not relative.endswith("source-attributions.json"):
                    legal_path = relative.split("/", 2)[2]
                    expected = next(e["sha256"] for e in legal[key]["legal_files"] if e["path"] == legal_path)
                    assert sha(original.read_bytes()) == expected
                elif relative.startswith("upstream/"):
                    supplements = json.loads((old / "upstream/supplements.json").read_text(encoding="utf-8"))
                    assert sha(original.read_bytes()) == next(e["sha256"] for e in supplements if e["file"] == Path(relative).name)
        elif key in additions:
            selected = additions[key]["license_chosen"]
            for entry in additions[key]["license_files"]:
                original = audit / entry["path"]
                assert sha(original.read_bytes()) == entry["sha256"]
                files.append((f"licences/cargo/{name}-{version}/" + Path(entry["path"]).name, original.read_bytes()))
        else:
            # Complément limité aux sources de résolution Linux non auditées
            # dans #50 ; on conserve leurs textes exacts, sans les dire liées.
            declared = package["license"] or ""
            normalized = declared.replace("/", " OR ")
            selected = "GPL-3.0-only" if "GPL-3.0-only" in normalized else "MIT" if "MIT" in normalized.split(" OR ") else normalized
            if selected not in ["MIT", "Apache-2.0", "BSD-3-Clause", "Unicode-3.0", "GPL-3.0-only", "Zlib"]:
                raise ValueError(f"Licence de source supplémentaire à examiner : {key}: {declared}")
            attributions = set()
            with tarfile.open(archive, "r:gz") as archive_file:
                for member in archive_file:
                    if member.isfile():
                        relative = member.name.split("/", 1)[1]
                        if member.size < 2_000_000:
                            data = archive_file.extractfile(member).read()
                            if b"\0" not in data:
                                for line in data.decode("utf-8", errors="replace").splitlines():
                                    if re.search(r"copyright\s*(?:\(c\)|©|[0-9]|:)|SPDX-FileCopyrightText", line, re.I) and len(line) < 600:
                                        attributions.add(line.strip())
                        if re.search(r"(^|/)(licen[cs]e[^/]*|copying[^/]*|notice[^/]*|copyright[^/]*)$|\.license$", relative, re.I):
                            files.append((f"licences/cargo/source-extra/{name}-{version}/{relative}", archive_file.extractfile(member).read()))
            if not files:
                files.extend(legal_texts(ROOT, source_supplements, key, {"primary", "attribution"}))
                if not files:
                    raise ValueError(f"Texte de licence source absent : {key}")
            files.extend(legal_texts(ROOT, source_supplements, key, {"additional"}))
            files.append((f"licences/cargo/source-extra/{name}-{version}/attributions.json",
                          (json.dumps(sorted(attributions), ensure_ascii=False, indent=2) + "\n").encode("utf-8")))
        if selected == "GPL-3.0-only" and not any(name.endswith("GPL-3.0-only.txt") for name, _ in files):
            files.append((f"licences/cargo/source-extra/{name}-{version}/GPL-3.0-only.txt", (ROOT / "LICENSE").read_bytes()))
        refs = []
        for relative, data in files:
            # Les chemins du corpus sont des données, jamais des chemins absolus.
            safe = PurePosixPath(relative)
            assert not safe.is_absolute() and ".." not in safe.parts
            save(source / relative, data)
            if key in roles:
                save(binary / relative, data)
            refs.append({"path": relative, "sha256": sha(data)})
        row = {"name": name, "version": version, "license_declared": package["license"], "license_selected": selected, "archive_sha256": checksum, "roles": sorted(roles.get(key, {"source-resolution-only-or-tests"})), "texts": refs}
        component_rows.append(row)
        line = f"{name} {version} | {selected} | {', '.join(row['roles'])}\n" + "\n".join("  " + e["path"] for e in refs)
        source_notices.append(line)
        if key in roles:
            binary_notices.append(line)
    sysroot = Path(run("rustc", "--print", "sysroot").decode().strip())
    runtime = sysroot / "share/doc/rust/COPYRIGHT-library.html"
    if sha(runtime.read_bytes()) != proof_hashes["rust-runtime/COPYRIGHT-library.html"]:
        raise ValueError("Le runtime Rust ne correspond pas au corpus #50.")
    # Ressources incorporées : même source, neuf SVG réellement présents dans cet ELF.
    resources = json.loads((old / "slint-resources.json").read_text())["resources"]
    compiler = next(p for k, p in metadata.items() if k[0] == "i-slint-compiler")
    for resource in resources:
        data = (Path(compiler["manifest_path"]).parent / resource["source"][0]).read_bytes()
        if sha(data) != resource["sha256"] or data not in (binary / "bunny-and-size").read_bytes():
            raise ValueError("Ressource Slint différente de l’audit : " + resource["symbol"])
    vendor_row, vendor_files, vendor_notice = local_backend_notice(ROOT)
    component_rows.append(vendor_row)
    binary_notices.append(vendor_notice)
    source_notices.append(vendor_notice)
    for folder in [binary, source]:
        for relative, raw in vendor_files:
            save(folder / relative, raw)
    for folder in [binary, source]:
        for relative in ["upstream/LICENSE.md", "upstream/GPL-3.0-only.txt", "upstream/REUSE.toml", "upstream/fluent-icons-LICENSE", "legal/winit-0.30.13/LICENSE"]:
            assert sha((old / relative).read_bytes()) == proof_hashes[relative]
            save(folder / "licences/resources" / relative, (old / relative).read_bytes())
        for path in (old / "rust-runtime").rglob("*"):
            if path.is_file():
                assert sha(path.read_bytes()) == proof_hashes[path.relative_to(old).as_posix()]
                save(folder / "licences/rust-runtime" / path.relative_to(old / "rust-runtime"), path.read_bytes())
        save(folder / "LICENSE", run("git", "show", commit + ":LICENSE"))
        for document in ["USER_GUIDE.md", "COMPATIBILITY.md", "RESERVATIONS.md"]:
            save(folder / document, run("git", "show", commit + ":docs/release/v0.3/" + document))
        for document in ["CLI.md", "ENTRIES_EXPORT.md", "GUI.md"]:
            save(folder / "docs" / document, run("git", "show", commit + ":docs/" + document))
    for folder in [binary, source]:
        save(folder / "MIGRATION_V03.md", run("git", "show", commit + ":docs/MIGRATION_V03.md"))
    for folder in [binary, source]:
        save(folder / "licences/artwork/CC-BY-SA-4.0.txt", run("git", "show", commit + ":assets/branding/CC-BY-SA-4.0.txt"))
        save(folder / "licences/artwork/PROVENANCE.json", run("git", "show", commit + ":assets/branding/PROVENANCE.json"))
    for folder in [binary, source]:
        for document in ["CREDITS.md", "BRANDING.md"]:
            save(folder / document, run("git", "show", commit + ":" + document))
    save(binary / "share/applications/bunny-and-size.desktop", run("git", "show", commit + ":packaging/linux/bunny-and-size.desktop"))
    for size in (16, 24, 32, 48, 64, 128, 256, 512):
        name = f"assets/branding/icons/bunny-and-size-{size}.png"
        save(binary / f"share/icons/hicolor/{size}x{size}/apps/bunny-and-size.png", run("git", "show", commit + ":" + name))
    # Les fichiers du projet viennent des blobs du commit, jamais d'un mélange
    # avec le checkout de #50 ou de modifications locales non commitées.
    for name in public_source_files(ROOT, commit):
        save(source / name, run("git", "show", commit + ":" + name))
    save(source / ".cargo/config.toml", '[source.crates-io]\nreplace-with = "linux-local"\n[source.linux-local]\nlocal-registry = "vendor-registry"\n[net]\noffline = true\n')
    save(source / "rebuild-linux.sh", (ROOT / "packaging/linux/rebuild-linux.sh").read_bytes())
    authors = sorted(set(run("git", "log", commit, "--format=%an", "--", "apps", "crates", "spikes").decode().splitlines()))
    authors_text = "Contributeurs du code original, tels que consignés par Git (pas une attribution exclusive des droits) :\n" + "\n".join(authors) + "\nLes composants tiers conservent leurs propres copyrights et licences.\n"
    notice_header = f"Bunny and Size {VERSION} — notices du candidat Linux x64\n\nCode original : GPL-3.0-only ; contributeurs recensés dans AUTHORS.txt.\nAUCUNE GARANTIE ; voir LICENSE pour les droits et conditions complets.\nSlint 1.18.1 : voie GPL-3.0-only, SixtyFPS GmbH et contributeurs.\nNeuf SVG Fluent : Microsoft Corporation, MIT ; complément Material Icons/Apache-2.0 conservé pour les contenus partagés.\nRuntime Rust 1.99.0 : disclosure et textes intégraux sous licences/rust-runtime.\nLes bibliothèques système Ubuntu sont des prérequis externes ; aucune copie redistribuée.\n\nRôles : runtime = candidat du graphe normal Linux ; production/build inclut les générateurs et outils de compilation du source correspondant.\nUne présence dans ce graphe ne prouve pas que tout le code du composant subsiste dans le binaire ELF.\nLes notices de source-résolution/tests ne sont pas présentées comme des dépendances binaires.\nLes textes originaux, copyrights et notices particuliers (AND Unicode/dpi, cfg_aliases) restent applicables.\n\n"
    notice_header += "Lapin adapté et dérivés : Louis Lejuez / TazmenWorld, CC BY-SA 4.0 ; adaptations et incorporation Slint/PE sous GPL-3.0-only (compatibilité unidirectionnelle). Voir BRANDING.md et licences/artwork/. Original complet et portrait exclus.\n\n"
    for folder, lines in [(binary, binary_notices), (source, source_notices)]:
        save(folder / "AUTHORS.txt", authors_text)
        save(folder / "THIRD_PARTY_NOTICES.txt", notice_header + "\n\n".join(lines) + "\n")
    provenance = {"version": VERSION, "candidate": True, "artwork_rights": artwork_rights, "redistribution_ready": artwork_rights["redistribution_ready"], "path_remapping": "/sortsize et /cargo", "target": TARGET, "source_commit": commit, "product_tree_sha256": product_identity(ROOT, commit), "audit_commit": AUDIT_COMMIT, "packager_sha256": sha(Path(__file__).read_bytes()), "rustc": run("rustc", "-Vv").decode(), "cargo": run("cargo", "-V").decode().strip(), "runtime_policy": "Bibliothèques système Ubuntu externes, voir README ; aucune bibliothèque système livrée", "lock_git_sha256": lock_hashes, "binaries": binaries, "source_registry_archives": len(metadata), "source_registry_index_versions": len(locked), "notice_components_linux_production_build": len(roles), "clean_linux_validation": "consulter le rapport externe docs/validation/issue-47-linux ; non exécutée par le packager", "source_legal_review": "docs/V01_INTEGRATION.md; archive dirty preserved, no incompatible handwritten delta identified; generated-bindings provenance not reproduced", "distribution_compliance": "consulter le rapport RC externe ; le packager ne certifie pas la recette"}
    write_json(source / "BUILDINFO.json", provenance)
    write_json(source / "SOURCE_COMPONENTS.json", component_rows)
    write_json(source / "SOURCE_LEGAL_SUPPLEMENTS.json", source_supplements)
    write_json(binary / "COMPONENTS.json", [row for row in component_rows if (row["name"], row["version"]) in roles or row.get("source")])
    save(source / "README.md", (ROOT / "packaging/linux/SOURCE_README.md").read_bytes())
    public_documentation(source)
    write_manifest(source)
    source_tar = output / (SOURCE_NAME + ".tar.gz")
    make_tar(source, source_tar, epoch)
    provenance["source_archive"] = {"filename": source_tar.name, "sha256": sha(source_tar.read_bytes())}
    write_json(binary / "BUILDINFO.json", provenance)
    save(binary / "README.md", (ROOT / "packaging/linux/README.md").read_bytes())
    # Manifeste de tous les fichiers permis ; seule la liste ci-dessus ajoute
    # des entrées. Aucun glob de target/release, PDB, log, DLL ou cache.
    allowed = [p.relative_to(binary).as_posix() for p in sorted(binary.rglob("*")) if p.is_file()]
    assert all(not name.lower().endswith((".dll", ".pdb", ".log")) for name in allowed)
    assert sorted(p.name for p in binary.iterdir() if p.is_file() and p.stat().st_mode & 0o111) == ["bunny-and-size", "bunny-and-size-cli", "sortsize"]
    public_documentation(binary)
    write_manifest(binary)
    binary_tar = output / (NAME + ".tar.gz")
    make_tar(binary, binary_tar, epoch)
    result = {"source_commit": commit, "archives": [{"path": str(p), "sha256": sha(p.read_bytes()), "bytes": p.stat().st_size} for p in [binary_tar, source_tar]], "binaries": binaries, "binary_file_count": len(allowed) + 2, "source_archive_count": len(metadata), "source_supplements_not_in_pr50": [list(k) for k in sorted(metadata.keys() - legal.keys() - additions.keys())]}
    write_json(output / "candidate.json", result)
    save(output / "SHA256SUMS", "".join(f"{e['sha256']}  {Path(e['path']).name}\n" for e in result["archives"]))
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
