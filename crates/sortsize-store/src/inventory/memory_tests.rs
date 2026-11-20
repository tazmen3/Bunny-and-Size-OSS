use super::*;
use sortsize_core::resources::{Category, Governor, MemoryReading};
const MIB: u64 = 1024 * 1024;
fn reading(available: u64) -> Option<MemoryReading> {
    Some(MemoryReading {
        total: 32 * 1024 * MIB,
        available,
    })
}
#[test]
fn low_memory_store_uses_admitted_cache_and_keeps_exact_transactions() {
    // The direct-scan policy admits up to half of available RAM. Keep these
    // scenarios at the same genuinely constrained 1/2/4 MiB SQLite capacities.
    for available in [16 * MIB, 32 * MIB, 64 * MIB, 256 * MIB] {
        let g = Governor::new(MemoryMode::Auto, reading(available), 0);
        if available < 128 * MIB {
            assert!(g.reserve(Category::Sqlite, 8 * MIB, None).is_none());
        }
        let admission = Store::reserve_memory_in(&g).unwrap();
        let bytes = admission.bytes();
        assert!(bytes <= g.usage().limits.categories[Category::Sqlite as usize]);
        let t = tempfile::tempdir().unwrap();
        let mut s = Store::create_admitted(
            &t.path().join("inventory"),
            InventoryId(1),
            SourceId(1),
            Durability::Retained,
            admission,
        )
        .unwrap();
        assert_eq!(s.sqlite_cache_bytes().unwrap(), bytes - 512 * 1024);
        let tx = s.db.transaction().unwrap();
        tx.execute_batch("CREATE TABLE probe(n INTEGER PRIMARY KEY,v INTEGER NOT NULL); WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<10000) INSERT INTO probe SELECT n,n*7 FROM nums;").unwrap();
        tx.commit().unwrap();
        let (n, sum): (i64, i64) =
            s.db.query_row("SELECT count(*),sum(v) FROM probe", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((n, sum), (10000, 350035000));
        assert_eq!(
            s.db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        drop(s);
        assert_eq!(g.usage().categories[Category::Sqlite as usize], 0);
    }
}
#[test]
fn true_pressure_refuses_before_creation_and_retry_recovers_without_restart() {
    let g = Governor::new(MemoryMode::Auto, reading(8 * MIB), 0);
    assert!(matches!(
        Store::reserve_memory_in(&g),
        Err(Error::MemoryAdmission {
            minimum: MIB,
            available: 524288,
            ..
        })
    ));
    g.configure(MemoryMode::Auto, reading(32 * MIB));
    let held = Store::reserve_memory_in(&g).unwrap();
    assert!(Store::reserve_memory_in(&g).is_err());
    g.configure(MemoryMode::Auto, reading(8 * MIB));
    assert_eq!(
        g.usage().categories[Category::Sqlite as usize],
        held.bytes()
    );
    assert!(Store::reserve_memory_in(&g).is_err());
    drop(held);
    g.configure(MemoryMode::Auto, reading(32 * MIB));
    assert!(Store::reserve_memory_in(&g).is_ok());
}
