//! The persisted cursor bounds returned rows and native SQLite VM work.
use super::*;

fn sweep(db: &Connection, parent: u64, after: u64) -> (Vec<(u64, u64)>, i32) {
    let mut q = db.prepare(RECONCILE_CHILDREN).unwrap();
    let rows = q
        .query_map(
            params![unsigned(0), unsigned(parent), unsigned(after), 512],
            |row| Ok((number(row, 0)?, number(row, 1)?)),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    (rows, q.get_status(rusqlite::StatementStatus::VmStep))
}

#[test]
fn reconcile_sweep_vm_work_does_not_grow_with_unrelated_children() {
    let mut db = Connection::open_in_memory().unwrap();
    db.execute_batch(SCHEMA).unwrap();
    db.execute(
        "INSERT INTO generations(id,context,revision,next_entry,aggregate_sum,aggregate_unknown) VALUES(?1,x'',?1,?1,?1,?1)",
        [unsigned(0)],
    )
    .unwrap();
    let target = (0..1000u64)
        .map(|i| (10 + 4 * i, i % 3 + 1))
        .collect::<Vec<_>>();
    let insert = "INSERT INTO entries(generation,id,parent,name,kind,attempt,present,metadata_known) VALUES(?1,?2,?3,?2,1,?4,?5,1)";
    {
        let tx = db.transaction().unwrap();
        {
            let mut q = tx.prepare(insert).unwrap();
            for &(id, attempt) in &target {
                // Presence must not filter the key sweep: old occurrences
                // and differing attempts still participate in reconciliation.
                q.execute(params![
                    unsigned(0),
                    unsigned(id),
                    unsigned(1),
                    unsigned(attempt),
                    id % 3 != 0
                ])
                .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let boundaries = [0, target[511].0, target[999].0];
    let before = boundaries.map(|after| sweep(&db, 1, after));
    assert_eq!(before[0].0, target[..512]);
    assert_eq!(before[1].0, target[512..]);
    assert!(before[2].0.is_empty());
    let absent_before = sweep(&db, 99, 0);
    {
        let tx = db.transaction().unwrap();
        {
            let mut q = tx.prepare(insert).unwrap();
            // Another parent interleaves the target IDs and extends well
            // past its EOF. A generation/id scan visits this unrelated tail.
            for id in 3..40_010u64 {
                if (10..=target[999].0).contains(&id) && (id - 10).is_multiple_of(4) {
                    continue;
                }
                q.execute(params![
                    unsigned(0),
                    unsigned(id),
                    unsigned(2),
                    unsigned(1),
                    true
                ])
                .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    for (index, after) in boundaries.into_iter().enumerate() {
        let (actual, steps) = sweep(&db, 1, after);
        assert_eq!(actual, before[index].0);
        assert!(
            steps <= before[index].1 * 2 + 100,
            "unrelated-parent scan: after={after}, before={}, after={steps}",
            before[index].1,
        );
    }
    let absent_after = sweep(&db, 99, 0);
    assert_eq!(absent_before.0, absent_after.0);
    assert!(absent_after.1 <= absent_before.1 * 2 + 100);
}
