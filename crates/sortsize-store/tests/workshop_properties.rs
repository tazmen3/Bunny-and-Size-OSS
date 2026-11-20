//! Native acquisition and preservation of explicitly selected Workshop fields.
use sortsize_core::{
    MetadataValue as M,
    acquisition::{Guard, Policy},
    inventory_contract::*,
};
use sortsize_store::{
    Durability,
    inventory::{Store, folder_totals::FolderFold},
    scheduler::{CollectionOptions, Session, State},
};
use std::time::{Duration, Instant};
fn context() -> AcquisitionContext {
    AcquisitionContext {
        inventory: InventoryId(118),
        source: SourceId(91),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    }
}
fn finish(s: &mut Session) {
    let start = Instant::now();
    loop {
        let state = s.tick().unwrap().state;
        if matches!(state, State::Idle | State::Cancelled) {
            s.finalize_step().unwrap();
        }
        if s.status().state == State::Sealed {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn native_options_survive_reopen_and_explicit_new_measurement() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), b"same bytes").unwrap();
    std::fs::hard_link(root.join("file"), root.join("alias")).unwrap();
    for enabled in [false, true] {
        let dir = t.path().join(format!("store-{enabled}"));
        let store = Store::create(
            &dir,
            context().inventory,
            context().source,
            Durability::Retained,
        )
        .unwrap();
        let mut session = Session::new_with_options(
            store,
            context(),
            Guard::open(&root, Policy::default()).unwrap(),
            CollectionOptions {
                allocation: enabled,
                owners: enabled,
            },
        )
        .unwrap();
        session
            .store_mut()
            .record_acquisition(context(), &root, 1)
            .unwrap();
        finish(&mut session);
        session
            .store_mut()
            .record_measurement(context().generation, &root, 1, 2)
            .unwrap();
        drop(session);
        let store = Store::reopen(&dir, Durability::Retained).unwrap();
        assert_eq!(
            store.collection_options(context().generation).unwrap(),
            Some((enabled, enabled))
        );
        let mut f = FolderFold::default();
        while !store
            .folder_fold_step(context().generation, &mut f)
            .unwrap()
        {}
        let total = store
            .folder_measure(context().generation, EntryId(0), &f)
            .unwrap()
            .unwrap();
        assert_eq!(total.files, 2);
        assert_eq!(total.allocated.is_some(), enabled);
        assert_eq!(total.allocation_unknown, if enabled { 0 } else { 2 });
        for id in 1..=2 {
            let p = store
                .entry_properties(EntryKey {
                    inventory: context().inventory,
                    generation: context().generation,
                    entry: EntryId(id),
                })
                .unwrap();
            assert_eq!(matches!(p.allocated, M::Known { .. }), enabled);
            assert_eq!(matches!(p.owner.id, M::Known { .. }), enabled);
            assert!(matches!(p.accessed, M::Known { .. }));
        }
        let mut verified = Session::verify(
            store,
            context().generation,
            Guard::open(&root, Policy::default()).unwrap(),
        )
        .unwrap();
        finish(&mut verified);
        let c = verified.context();
        assert_eq!(
            verified.store().collection_options(c.generation).unwrap(),
            Some((enabled, enabled))
        );
        assert_ne!(c.generation, context().generation);
    }
}
