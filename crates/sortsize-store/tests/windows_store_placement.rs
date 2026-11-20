//! P0 Windows : placement sûr du magasin, sans parcourir les racines personnelles.
#![cfg(windows)]

use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{
        Store,
        placement::{self, PlacementError},
    },
    scheduler::{Session, State},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn temporary_directory_in(
    guard: &Guard,
    candidates: &[PathBuf],
) -> Result<tempfile::TempDir, PlacementError> {
    placement::temporary_directory_in(guard, "bunny-v05-p0-", candidates)
}

fn guard(path: &Path) -> Guard {
    Guard::open(path, Policy::default()).unwrap()
}

#[test]
fn source_candidates_are_rejected_before_any_application_directory_is_created() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let nested = source.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let before = fs::read_dir(&source).unwrap().count();
    assert!(temporary_directory_in(&guard(&source), &[source.clone(), nested.clone()]).is_err());
    assert_eq!(fs::read_dir(&source).unwrap().count(), before);
    assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
}

#[test]
fn case_and_verbatim_aliases_do_not_bypass_exclusion_or_accept_parent_components() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("Source-Mixed");
    let external = fixture.path().join("external");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&external).unwrap();
    let acquired = guard(&source);
    let lower = PathBuf::from(source.to_string_lossy().to_lowercase());
    let verbatim = fs::canonicalize(&source).unwrap();
    let parent_component = external.join("..").join("external");
    assert!(!acquired.store_outside(&lower).unwrap());
    assert!(!acquired.store_outside(&verbatim).unwrap());
    assert!(temporary_directory_in(&acquired, &[lower, verbatim, parent_component]).is_err());
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&external).unwrap().count(), 0);
}

#[test]
fn reparse_parent_is_rejected_without_following_it_and_next_candidate_works() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let external = fixture.path().join("external");
    let alias = fixture.path().join("junction");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&external).unwrap();
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "New-Item -ItemType Junction -Path $env:BUNNYSIZE_P0_LINK -Target $env:BUNNYSIZE_P0_TARGET -ErrorAction Stop | Out-Null",
        ])
        .env("BUNNYSIZE_P0_LINK", &alias)
        .env("BUNNYSIZE_P0_TARGET", &source)
        .env_remove("PSModulePath")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let acquired = guard(&source);
    assert!(acquired.store_outside(&alias).is_err());
    let temporary = temporary_directory_in(&acquired, &[alias, external.clone()]).unwrap();
    assert!(acquired.store_outside(temporary.path()).unwrap());
    assert!(temporary.path().starts_with(&external));
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    // Every object, including the junction target, is inside this owned fixture.
}

#[test]
fn existing_application_data_is_preserved() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let external = fixture.path().join("external");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&external).unwrap();
    let acquired = guard(&source);
    let first = temporary_directory_in(&acquired, std::slice::from_ref(&external)).unwrap();
    let container = first.path().parent().unwrap().to_owned();
    let preserved = container.join("preserve-existing-cache.bin");
    fs::write(&preserved, b"existing cache must remain untouched").unwrap();
    drop(first);
    let second = temporary_directory_in(&acquired, &[external]).unwrap();
    assert!(second.path().starts_with(&container));
    assert_eq!(
        fs::read(&preserved).unwrap(),
        b"existing cache must remain untouched"
    );
}

#[test]
fn invalid_file_candidate_is_preserved_and_does_not_block_the_fallback() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let invalid = fixture.path().join("not-a-directory");
    let external = fixture.path().join("external");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(&invalid, b"preserve this file").unwrap();
    let acquired = guard(&source);
    let temporary =
        temporary_directory_in(&acquired, &[invalid.clone(), external.clone()]).unwrap();
    assert!(acquired.store_outside(temporary.path()).unwrap());
    assert!(temporary.path().starts_with(&external));
    assert_eq!(fs::read(invalid).unwrap(), b"preserve this file");
}

#[test]
fn unsuitable_existing_acl_is_refused_without_rewriting_the_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let unsafe_parent = fixture.path().join("unsafe-parent");
    let external = fixture.path().join("external");
    let unsafe_container = unsafe_parent.join(".bunny-and-size-temporary-v1");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&unsafe_container).unwrap();
    fs::create_dir(&external).unwrap();
    let preserved = unsafe_container.join("preserve-existing-cache.bin");
    fs::write(&preserved, b"existing data in an untrusted directory").unwrap();
    // The ordinary inherited ACL has not been created by the application's
    // protected owner/SYSTEM policy and must never be silently replaced.
    // Windows PowerShell 5's filesystem provider does not accept the verbatim
    // spelling returned by tempfile. Normalize only this short fixture path
    // for Get-Acl; product paths and the exclusion checks keep native spelling.
    let read_acl = r"$p = $env:BUNNYSIZE_P0_ACL; if ($p.StartsWith('\\?\UNC\')) { $p = '\\' + $p.Substring(8) } elseif ($p.StartsWith('\\?\')) { $p = $p.Substring(4) }; (Get-Acl -LiteralPath $p -ErrorAction Stop).Sddl";
    let before = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            read_acl,
        ])
        .env("BUNNYSIZE_P0_ACL", &unsafe_container)
        // Let Windows PowerShell reconstruct its own module search path,
        // independent of the PowerShell 7 runner's inherited module path.
        .env_remove("PSModulePath")
        .output()
        .unwrap();
    assert!(
        before.status.success(),
        "Get-Acl before placement: {}",
        String::from_utf8_lossy(&before.stderr)
    );
    assert!(!before.stdout.is_empty(), "Get-Acl returned no SDDL");
    let acquired = guard(&source);
    let temporary = temporary_directory_in(&acquired, &[unsafe_parent, external.clone()]).unwrap();
    assert!(temporary.path().starts_with(&external));
    assert_eq!(
        fs::read(preserved).unwrap(),
        b"existing data in an untrusted directory"
    );
    let after = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            read_acl,
        ])
        .env("BUNNYSIZE_P0_ACL", &unsafe_container)
        .env_remove("PSModulePath")
        .output()
        .unwrap();
    assert!(
        after.status.success(),
        "Get-Acl after placement: {}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(before.stdout, after.stdout);
}

#[test]
fn selected_external_store_starts_and_completes_a_small_real_scan() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let external = fixture.path().join("external");
    fs::create_dir_all(source.join("sous-dossier-é")).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(source.join("racine.txt"), b"1234567").unwrap();
    fs::write(source.join("sous-dossier-é/texte.txt"), b"abc").unwrap();
    fs::write(source.join("sous-dossier-é/vide.bin"), []).unwrap();
    let acquired = guard(&source);
    let temporary = temporary_directory_in(&acquired, &[source.clone(), external]).unwrap();
    let context = AcquisitionContext {
        inventory: InventoryId(831),
        source: SourceId(831),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    let store = Store::create(
        &temporary.path().join("inventory"),
        context.inventory,
        context.source,
        Durability::Reconstructible,
    )
    .unwrap();
    let mut session = Session::new(store, context, acquired).unwrap();
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(15));
        let state = session.tick().unwrap().state;
        assert_ne!(state, State::StoreError);
        if state == State::Idle {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(15));
    }
    assert_eq!(session.status().state, State::Sealed);
    assert_eq!(session.store().entry_count(context.generation).unwrap(), 5);
    assert_eq!(
        session.store().totals(context.generation).unwrap().logical,
        Some(10)
    );
    assert!(
        session
            .can_place_store(session.store().directory())
            .unwrap()
    );
}

#[test]
#[ignore = "opt-in Windows physique avec volumes C et D : prévol sans énumération"]
fn physical_roots_accept_an_external_store_without_enumerating_personal_data() {
    let on_c = tempfile::Builder::new()
        .prefix("bunny-size-p0-physical-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let on_d = tempfile::Builder::new()
        .prefix(".bunny-size-p0-physical-")
        .tempdir_in(r"D:\")
        .unwrap();
    assert!(on_c.path().starts_with(r"C:\Users"));
    let candidates = [on_c.path().to_owned(), on_d.path().to_owned()];
    for root in [r"C:\Users", r"C:\", r"D:\", r"D:\Jeux"] {
        let acquired = guard(Path::new(root));
        let temporary = temporary_directory_in(&acquired, &candidates).unwrap();
        assert!(acquired.store_outside(temporary.path()).unwrap());
        if root == r"C:\Users" || root == r"C:\" {
            assert!(temporary.path().starts_with(on_d.path()));
        } else {
            assert!(temporary.path().starts_with(on_c.path()));
        }
        println!(
            "root={root}; temporary_store={}",
            temporary.path().display()
        );
    }
}

#[test]
fn no_valid_candidate_reports_a_retryable_error_without_creating_source_state() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    fs::create_dir(&source).unwrap();
    let missing = fixture.path().join("missing-parent");
    let error =
        temporary_directory_in(&guard(&source), &[source.clone(), missing.clone()]).unwrap_err();
    assert_eq!(error.root, source);
    assert_eq!(error.attempts.len(), 2);
    assert!(error.to_string().contains("BUNNY_AND_SIZE_STORE_DIR"));
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    assert!(!missing.exists());
}

#[test]
fn a_source_named_like_the_application_container_is_never_used_for_state() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join(".bunny-and-size-temporary-v1");
    fs::create_dir(&source).unwrap();
    assert!(temporary_directory_in(&guard(&source), &[fixture.path().to_owned()]).is_err());
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
}
