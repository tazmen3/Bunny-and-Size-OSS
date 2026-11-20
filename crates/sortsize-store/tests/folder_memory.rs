//! Isolated process: deterministic pressure on the shared SQLite admission budget.
use sortsize_core::{
    inventory_contract::*,
    resources::{Category, Governor},
};
use sortsize_store::{
    Durability,
    inventory::{Store, folder_totals::FolderFold},
};
#[test]
fn temporary_folder_database_is_charged_once_until_its_store_closes() {
    const MIB: u64 = 1024 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::create(
        &temp.path().join("store"),
        InventoryId(1),
        SourceId(1),
        Durability::Reconstructible,
    )
    .unwrap();
    let c = AcquisitionContext {
        inventory: InventoryId(1),
        source: SourceId(1),
        generation: GenerationId(1),
        scan: ScanId(1),
        policy: PolicyId(1),
        mount: MountEpoch(1),
        run: RunEpoch(1),
        validation: ValidationEpoch(1),
    };
    store.create_generation(c).unwrap();
    let governor = Governor::global();
    governor.configure(MemoryMode::Auto, None);
    let usage = governor.usage();
    let sql = Category::Sqlite as usize;
    let cache_before = store.sqlite_cache_bytes().unwrap();
    let within_store = cache_before + MIB / 2 >= 2 * MIB;
    let extra = if within_store { 0 } else { MIB };
    // A normally admitted Store lends from its main cache even with zero
    // remaining capacity. A 1 MiB Store really needs another 1 MiB admission.
    let pressure = governor
        .reserve(
            Category::Sqlite,
            usage.limits.categories[sql] - usage.categories[sql] - extra,
            None,
        )
        .unwrap();
    let before = governor.usage().categories[sql];
    {
        let mut first = FolderFold::default();
        while !store.folder_fold_step(c.generation, &mut first).unwrap() {}
        assert_eq!(governor.usage().categories[sql], before + extra);
        while !store.consolidate_step(c, 512).unwrap() {}
        store.seal(c).unwrap();
        assert_eq!(
            store.sqlite_cache_bytes().unwrap(),
            cache_before - if within_store { MIB } else { 0 }
        );
        let mut c2 = c;
        c2.generation = GenerationId(2);
        store.create_generation(c2).unwrap();
        let mut second = FolderFold::default();
        while !store.folder_fold_step(c2.generation, &mut second).unwrap() {}
        assert_eq!(governor.usage().categories[sql], before + extra);
        assert!(
            store
                .folder_measure(c.generation, EntryId(0), &first)
                .unwrap()
                .is_none()
        );
        while !store.folder_fold_step(c.generation, &mut first).unwrap() {}
    }
    // Dropping cursors must not uncharge the still-live SQLite TEMP tables.
    assert_eq!(governor.usage().categories[sql], before + extra);
    assert!(governor.reserve(Category::Sqlite, 1, None).is_none());
    drop(store);
    assert_eq!(governor.usage().categories[sql], pressure.bytes());
    drop(pressure);
    assert_eq!(governor.usage().categories[sql], 0);
}
