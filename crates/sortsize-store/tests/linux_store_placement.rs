//! Native Linux store placement and acquisition, on a private tiny corpus only.
#![cfg(target_os = "linux")]

use sortsize_core::{
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{Store, placement},
    scheduler::{Session, State},
};
use std::{
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, fs::symlink},
    time::{Duration, Instant},
};

#[test]
fn linux_private_external_store_preserves_posix_source_and_rejects_links() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let external = fixture.path().join("external");
    let alias = fixture.path().join("alias");
    fs::create_dir_all(source.join("sous-dossier-é")).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(source.join("racine.txt"), b"1234567").unwrap();
    fs::write(
        source
            .join("sous-dossier-é")
            .join(OsString::from_vec(b"natif-\xff".to_vec())),
        b"abc",
    )
    .unwrap();
    symlink(&external, &alias).unwrap();
    let guard = Guard::open(&source, Policy::default()).unwrap();
    assert!(
        placement::temporary_directory_in(&guard, "linux-parity-", &[source.clone(), alias])
            .is_err()
    );
    assert!(!external.join(".bunny-and-size-temporary-v1").exists());
    let temporary =
        placement::temporary_directory_in(&guard, "linux-parity-", &[external]).unwrap();
    // tempfile children inherit the caller umask; the private container is
    // the security boundary and must deny all group/other traversal.
    assert_eq!(
        fs::metadata(temporary.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let context = AcquisitionContext {
        inventory: InventoryId(832),
        source: SourceId(832),
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
    let mut session = Session::new(store, context, guard).unwrap();
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
    assert_eq!(session.store().entry_count(context.generation).unwrap(), 4);
    assert_eq!(
        session.store().totals(context.generation).unwrap().logical,
        Some(10)
    );
    assert!(
        session
            .can_place_store(session.store().directory())
            .unwrap()
    );
    assert_eq!(fs::read_dir(&source).unwrap().count(), 2);
    assert!(!source.join(".bunny-and-size-temporary-v1").exists());
}

#[test]
fn linux_existing_untrusted_state_is_preserved_and_fallback_is_private() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let unsafe_parent = fixture.path().join("unsafe-parent");
    let unsafe_container = unsafe_parent.join(".bunny-and-size-temporary-v1");
    let external = fixture.path().join("external");
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&unsafe_container).unwrap();
    fs::set_permissions(&unsafe_container, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(unsafe_container.join("preserved"), b"existing local state").unwrap();
    fs::create_dir(&external).unwrap();
    let guard = Guard::open(&source, Policy::default()).unwrap();
    let temporary = placement::temporary_directory_in(
        &guard,
        "linux-parity-",
        &[unsafe_parent, external.clone()],
    )
    .unwrap();
    assert!(temporary.path().starts_with(external));
    assert_eq!(
        fs::metadata(&unsafe_container)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::read(unsafe_container.join("preserved")).unwrap(),
        b"existing local state"
    );
}
