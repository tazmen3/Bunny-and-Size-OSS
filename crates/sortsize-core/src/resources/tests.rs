use super::*;
use crate::inventory_contract::{EntryId, GenerationId, InventoryId};
fn key(i: u64) -> EntryKey {
    EntryKey {
        inventory: InventoryId(1),
        generation: GenerationId(1),
        entry: EntryId(i),
    }
}
#[test]
fn publication_releases_unused_capacity_without_growing_or_double_release() {
    let g = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let mut retained = g.reserve(Category::Publications, 512 * 1024, None).unwrap();
    assert!(retained.shrink_to(32 * 1024));
    assert_eq!(
        g.usage().categories[Category::Publications.index()],
        32 * 1024
    );
    assert!(!retained.shrink_to(64 * 1024));
    assert_eq!(retained.bytes(), 32 * 1024);
    drop(retained);
    assert_eq!(g.usage().categories[Category::Publications.index()], 0);
}

#[test]
fn partition_admission_is_atomic_and_minima_can_share_unused_process_memory() {
    let governor = Governor::new(MemoryMode::Auto, None, 0);
    let parts = [
        (Category::Queries, 2 * MIB, 2 * MIB),
        (Category::Publications, 3 * MIB, 3 * MIB),
        (Category::Transport, MIB, 4 * MIB),
    ];
    assert!(governor.reserve_partition(6 * MIB - 1, parts).1.is_none());
    assert_eq!(governor.usage().categories, [0; 6]);
    let first = governor.reserve_partition(7 * MIB, parts).1.unwrap();
    let second = governor.reserve_partition(7 * MIB, parts).1.unwrap();
    assert_eq!(
        governor.usage().categories[Category::Publications.index()],
        6 * MIB
    );
    assert_eq!(first.iter().map(Reservation::bytes).sum::<u64>(), 7 * MIB);
    drop(first);
    assert_eq!(
        governor.usage().categories[Category::Publications.index()],
        3 * MIB
    );
    drop(second);
    assert_eq!(governor.usage().categories, [0; 6]);
}

#[test]
fn named_prospective_disk_reservations_only_compete_on_their_volume() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let a = DiskVolume::fixture(1);
    let b = DiskVolume::fixture(2);
    let unknown = governor.reserve_disk(MIB).unwrap();
    let volume_a = governor
        .reserve_disk_inner(2 * MIB, None, Some(a.clone()))
        .unwrap();
    let _volume_b = governor
        .reserve_disk_inner(4 * MIB, None, Some(b.clone()))
        .unwrap();
    assert_eq!(governor.prospective_disk_bytes(Some(&a)), 3 * MIB);
    assert_eq!(governor.prospective_disk_bytes(Some(&b)), 5 * MIB);
    assert_eq!(governor.prospective_disk_bytes(None), 7 * MIB);
    drop(unknown);
    drop(volume_a);
    assert_eq!(governor.prospective_disk_bytes(Some(&a)), 0);
    assert_eq!(governor.prospective_disk_bytes(Some(&b)), 4 * MIB);
}
#[test]
fn six_modes_count_and_bytes_and_missing_memory_are_distinct() {
    for mode in [
        MemoryMode::Auto,
        MemoryMode::Entries50k,
        MemoryMode::Entries250k,
        MemoryMode::Entries1m,
        MemoryMode::Entries5m,
        MemoryMode::UnlimitedCount,
    ] {
        let l = Limits::calibrate(mode, None);
        assert!(l.fallback);
        assert_eq!(l.bytes, 128 * MIB);
        assert!(l.categories.iter().sum::<u64>() <= l.bytes);
        assert_eq!(l.requested, mode);
    }
    assert_eq!(
        Limits::calibrate(MemoryMode::UnlimitedCount, None).entries,
        None
    );
    assert_eq!(
        Limits::calibrate(
            MemoryMode::Auto,
            Some(MemoryReading {
                total: 8 * 1024 * MIB,
                available: 4 * 1024 * MIB
            })
        )
        .bytes,
        2048 * MIB
    );
    assert_eq!(disk_limit(512 * MIB), 0);
    assert_eq!(disk_limit(600 * MIB), 88 * MIB);
}
#[test]
fn global_accounting_counts_distinct_keys_and_all_pinned_copies() {
    let g = Governor::new(MemoryMode::Entries50k, None, 8 * 1024 * MIB);
    let a = g.reserve(Category::Cache, 1024, Some(key(1))).unwrap();
    let b = g.reserve(Category::Cache, 2048, Some(key(1))).unwrap();
    assert_eq!(g.usage().distinct_entries, 1);
    assert_eq!(g.usage().categories[0], 3072);
    drop(a);
    assert_eq!(g.usage().distinct_entries, 1);
    drop(b);
    assert_eq!(g.usage().distinct_entries, 0);
    let disk = g.reserve_disk(1024 * MIB).unwrap();
    assert!(g.reserve_disk(2 * 1024 * MIB).is_none());
    g.observe_disk(600 * MIB, 100 * MIB);
    assert!(g.reserve_disk(1).is_none());
    drop(disk);
    assert_eq!(g.usage().disk_reserved, 0);
}
#[test]
fn shrink_does_not_remove_pinned_pages_and_growth_does_not_preallocate() {
    let g = Governor::new(MemoryMode::Auto, None, 0);
    let mut c = Cache::new(g.clone());
    c.insert(key(1), 1, 1024, true).unwrap();
    c.insert(key(2), 2, 1024, false).unwrap();
    g.configure(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 1,
            available: 0,
        }),
    );
    c.trim();
    assert_eq!(c.len(), 1);
    assert_eq!(c.get(key(1)), Some(&1));
    assert!(c.insert(key(3), 3, 1, false).is_err());
    c.pin(key(1), false);
    c.trim();
    assert!(c.is_empty());
    g.configure(MemoryMode::Entries5m, None);
    assert!(c.is_empty());
}
#[test]
fn accounting_plateaus_with_one_and_ten_million_admissions() {
    let g = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 32768,
            available: 8192,
        }),
        0,
    );
    let mut c = Cache::new(g.clone());
    let mut one = None;
    for i in 0..10_000_000 {
        let _ = c.insert(key(i), i, 8, false);
        if i == 999_999 {
            one = Some(g.usage().categories[0]);
        }
    }
    assert_eq!(g.usage().categories[0], one.unwrap());
    assert!(c.len() <= 8);
}

#[test]
fn automatic_budget_is_adaptive_without_preallocation_and_ceiling_survives_refresh() {
    let reading = MemoryReading {
        total: 64 * 1024 * MIB,
        available: 32 * 1024 * MIB,
    };
    assert_eq!(
        Limits::calibrate(MemoryMode::Auto, Some(reading)).bytes,
        4096 * MIB
    );
    let g = Governor::new(MemoryMode::Auto, Some(reading), 0);
    assert_eq!(g.usage().categories, [0; 6]);
    g.set_byte_limit(Some(256 * MIB));
    g.configure(MemoryMode::Auto, Some(reading));
    assert_eq!(g.usage().limits.bytes, 256 * MIB);
    g.refresh_native();
    assert!(g.usage().limits.bytes <= 256 * MIB);
    g.set_byte_limit(None);
    g.configure(MemoryMode::Auto, Some(reading));
    assert_eq!(g.usage().limits.bytes, 4096 * MIB);
}
#[test]
fn native_reading_or_explicit_missing() {
    if let Some(m) = native_memory() {
        assert!(m.total > 0 && m.available <= m.total);
    }
}

#[test]
fn deleted_groups_release_registry_and_quota_but_closed_groups_remain_charged() {
    let g = Governor::new(MemoryMode::Auto, None, 8 * 1024 * 1024 * 1024);
    for n in 0..1100 {
        let path = std::path::PathBuf::from(format!("owned-{n}"));
        assert!(g.observe_artifact_group(&path, 8 * 1024 * 1024 * 1024, 1024));
        assert_eq!(g.usage().disk_used, 1024);
        g.forget_deleted_artifact_group(&path);
        assert_eq!(g.usage().disk_used, 0);
    }
    let retained = std::path::Path::new("retained");
    assert!(g.observe_artifact_group(retained, 8 * 1024 * 1024 * 1024, 4096));
    assert_eq!(g.usage().disk_used, 4096);
}

#[test]
fn artifact_guard_blocks_every_downward_notification_and_transfers_prospective_bytes() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let root = std::path::Path::new("/owned-canonical-generation");
    let mut prospective = governor.reserve_disk(100 * MIB).unwrap();
    let guard = governor
        .guard_artifact_group(root, 8 * 1024 * MIB, 0)
        .unwrap();
    assert!(guard.admit(40 * MIB, Some(&mut prospective)));
    assert_eq!(governor.usage().disk_used, 40 * MIB);
    assert_eq!(governor.usage().disk_reserved, 60 * MIB);
    assert!(governor.observe_artifact_group(root, 8 * 1024 * MIB, 0));
    governor.observe_disk(8 * 1024 * MIB, 0);
    governor.forget_deleted_artifact_group(root);
    assert_eq!(governor.usage().disk_used, 40 * MIB);
    assert!(guard.admit(100 * MIB, Some(&mut prospective)));
    assert_eq!(governor.usage().disk_reserved, 0);
    // Falling quota does not delete the valid data or permit one extra byte.
    governor.observe_disk(600 * MIB, 0);
    assert!(!guard.admit(100 * MIB + 1, None));
    assert_eq!(governor.usage().disk_used, 100 * MIB);
    guard.finish(90 * MIB);
    assert_eq!(governor.usage().disk_used, 90 * MIB);
    governor.forget_deleted_artifact_group(root);
    assert_eq!(governor.usage().disk_used, 0);
}
#[test]
fn failed_artifact_owner_drop_is_sticky_and_cross_governor_tokens_cannot_pay() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let other = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let root = std::path::Path::new("/failed-native-close");
    let guard = governor
        .guard_artifact_group(root, 8 * 1024 * MIB, 0)
        .unwrap();
    let mut wrong = other.reserve_disk(MIB).unwrap();
    assert!(!guard.admit(MIB, Some(&mut wrong)));
    assert!(guard.admit(MIB, None));
    drop(guard);
    governor.observe_artifact_group(root, 8 * 1024 * MIB, 0);
    governor.observe_disk(8 * 1024 * MIB, 0);
    governor.forget_deleted_artifact_group(root);
    assert_eq!(governor.usage().disk_used, MIB);
    assert!(
        governor
            .guard_artifact_group(root, 8 * 1024 * MIB, MIB)
            .is_none()
    );
}

#[test]
fn aggregate_observation_cannot_erase_other_retained_groups_while_a_ledger_is_live() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    governor.observe_artifact_group(
        std::path::Path::new("/other-retained"),
        8 * 1024 * MIB,
        100 * MIB,
    );
    let guard = governor
        .guard_artifact_group(std::path::Path::new("/live"), 8 * 1024 * MIB, 40 * MIB)
        .unwrap();
    governor.observe_disk(8 * 1024 * MIB, 0);
    assert_eq!(governor.usage().disk_used, 140 * MIB);
    assert!(guard.admit(50 * MIB, None));
    assert_eq!(governor.usage().disk_used, 150 * MIB);
    guard.finish(50 * MIB);
}

#[test]
fn memory_admission_checks_the_global_limit_after_native_capacity_shrinks() {
    let governor = Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 8 * 1024 * MIB,
            available: 8 * 1024 * MIB,
        }),
        8 * 1024 * MIB,
    );
    let pinned = governor.reserve(Category::Sqlite, 16 * MIB, None).unwrap();
    governor.configure(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 8 * 1024 * MIB,
            available: 16 * MIB,
        }),
    );
    assert!(governor.usage().limits.bytes < 16 * MIB);
    assert!(governor.reserve(Category::Queries, 1, None).is_none());
    assert!(
        governor
            .reserve_up_to(Category::Publications, 1, MIB)
            .is_none()
    );
    drop(pinned);
    assert!(governor.reserve(Category::Queries, 1, None).is_some());
}
#[test]
fn group_bound_operation_lease_is_converted_without_double_charging() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let root = std::path::Path::new("/operation-target");
    let guard = governor
        .guard_artifact_group(root, 8 * 1024 * MIB, 0)
        .unwrap();
    let token = governor.reserve_disk_for_group(root, 32 * MIB).unwrap();
    assert!(guard.admit(24 * MIB, None));
    assert_eq!(token.bytes(), 8 * MIB);
    assert_eq!(
        governor.usage().disk_used + governor.usage().disk_reserved,
        32 * MIB
    );
    drop(token);
    assert_eq!(governor.usage().disk_reserved, 0);
    guard.finish(24 * MIB);
}

#[test]
fn closed_real_artifacts_stay_charged_until_confirmed_removal_and_rename_is_atomic() {
    let governor = Governor::new(MemoryMode::Auto, None, 8 * 1024 * MIB);
    let temporary = tempfile::tempdir().unwrap();
    let stage = temporary.path().join("stage");
    let destination = temporary.path().join("published");
    std::fs::create_dir(&stage).unwrap();
    std::fs::write(stage.join("inventory.db"), [1u8; 4096]).unwrap();
    let guard = governor
        .guard_artifact_group(&stage, 8 * 1024 * MIB, 4096)
        .unwrap();
    guard.finish(4096);
    governor.observe_artifact_group(&stage, 8 * 1024 * MIB, 0);
    governor.observe_disk(8 * 1024 * MIB, 0);
    governor.forget_deleted_artifact_group(&stage);
    assert_eq!(governor.usage().disk_used, 4096);
    std::fs::rename(&stage, &destination).unwrap();
    assert!(governor.relocate_artifact_group(&stage, &destination));
    assert_eq!(governor.usage().disk_used, 4096);
    governor.observe_artifact_group(&stage, 8 * 1024 * MIB, 0);
    governor.observe_artifact_group(&destination, 8 * 1024 * MIB, 0);
    governor.forget_deleted_artifact_group(&destination);
    assert_eq!(governor.usage().disk_used, 4096);
    std::fs::remove_file(destination.join("inventory.db")).unwrap();
    governor.forget_deleted_artifact_group(&destination);
    assert_eq!(governor.usage().disk_used, 0);
}
