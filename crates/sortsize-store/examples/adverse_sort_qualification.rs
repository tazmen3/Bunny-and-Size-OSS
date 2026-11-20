//! Targeted qualification of the integrated engine. Fixture mutation is allowed
//! ONLY for a marked, private copy, never for the original historical corpus.
#[path = "support/adverse.rs"]
mod support;
use rusqlite::{Connection, params};
use sortsize_core::{CancellationToken, inventory_contract::*, resources::Governor};
use sortsize_store::{
    Durability,
    inventory::{
        Kind, Store,
        preview::Unverified,
        queries::{Cursor, Page},
    },
};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use support::{memory, value};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn prepare(dir: &Path, n: u64, pattern: &str) -> Result<()> {
    let marker = dir
        .parent()
        .ok_or("fixture parent")?
        .join("SYNTHETIC_ADVERSE_ONLY");
    let original = PathBuf::from(std::fs::read_to_string(marker)?.trim());
    if dir.canonicalize()? == original.canonicalize()? {
        return Err("original corpus cannot be mutated".into());
    }
    let start = Instant::now();
    let store = Store::reopen(dir, Durability::Retained)?;
    let saved = store.measurement(GenerationId(1))?;
    assert_eq!(store.entry_count(GenerationId(1))?, n + 1);
    drop(store);
    let mut db = Connection::open_with_flags(
        dir.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA mmap_size=0;PRAGMA cache_size=-4096;PRAGMA temp_store=FILE;PRAGMA threads=1;")?;
    assert_eq!(db.query_row::<i64,_,_>("SELECT count(*) FROM entries WHERE generation=x'0000000000000001' AND kind=1 AND present=1",[],|r|r.get(0))?,n as i64);
    db.execute_batch("BEGIN;UPDATE generations SET sealed=0,aggregate_cursor=NULL,aggregate_sum=x'0000000000000000',aggregate_unknown=x'0000000000000000',aggregate_overflow=0,aggregate_done=0,coverage=0;UPDATE identity SET latest=NULL,latest_complete=NULL;DROP INDEX entries_size;COMMIT;")?;
    let (mut sum, mut unknown) = (0u128, 0u64);
    for first in (0..n).step_by(50_000) {
        let tx = db.transaction()?;
        {
            let mut e = tx.prepare(
                "UPDATE entries SET logical=?1 WHERE generation=x'0000000000000001' AND id=?2",
            )?;
            let mut c=tx.prepare("UPDATE contributions SET logical=?1 WHERE generation=x'0000000000000001' AND id=?2")?;
            for i in first..(first + 50_000).min(n) {
                let logical = value(pattern, i, n);
                if let Some(v) = logical {
                    sum += u128::from(v);
                } else {
                    unknown += 1;
                }
                let bytes = logical.map(u64::to_be_bytes);
                let id = (i + 1).to_be_bytes();
                assert_eq!(
                    e.execute(params![bytes.as_ref().map(|v| v.as_slice()), id])?,
                    1
                );
                assert_eq!(
                    c.execute(params![bytes.as_ref().map(|v| v.as_slice()), id])?,
                    1
                );
            }
        }
        tx.commit()?;
    }
    db.execute_batch("CREATE INDEX entries_size ON entries(generation,logical,id);PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(db);
    let mut store = Store::reopen(dir, Durability::Retained)?;
    while !store.consolidate_step(saved.context, 512)? {}
    let totals = store.totals(GenerationId(1))?;
    assert_eq!(totals.logical, u64::try_from(sum).ok());
    assert_eq!(totals.unknown, unknown);
    assert_eq!(totals.overflow, sum > u128::from(u64::MAX));
    let coverage = store.seal(saved.context)?;
    assert_eq!(
        coverage == ScopeCoverage::Complete,
        unknown == 0 && sum <= u128::from(u64::MAX)
    );
    assert_eq!(store.measurement(GenerationId(1))?.root, saved.root);
    drop(store);
    let db = Connection::open_with_flags(
        dir.join("inventory.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    println!(
        "{}",
        serde_json::json!({"phase":"prepare","synthetic_copy":true,"original":original,"files":n,"pattern":pattern,"seed":"9e3779b97f4a7c15","sum_u128":sum.to_string(),"unknown_files":unknown,"coverage":format!("{coverage:?}"),"seconds":start.elapsed().as_secs_f64(),"peak_process_bytes":memory(),"source_acquisition":false})
    );
    Ok(())
}

fn verify_pages(
    mut read: impl FnMut(Option<&Cursor>) -> sortsize_store::inventory::Result<Page>,
    root: &Path,
    n: u64,
    pattern: &str,
    descending: bool,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    let mut seen = vec![0u8; ((n + 1).div_ceil(8)) as usize];
    let mut previous: Option<(Option<u64>, u64)> = None;
    let mut cursor = None;
    let mut count = 0u64;
    let mut max_page_ms = 0f64;
    loop {
        let t = Instant::now();
        let page = read(cursor.as_ref())?;
        max_page_ms = max_page_ms.max(t.elapsed().as_secs_f64() * 1000.);
        for row in &page.rows {
            let id = row.key.entry.0;
            assert!(id <= n);
            let (slot, bit) = ((id / 8) as usize, 1u8 << (id % 8));
            assert_eq!(seen[slot] & bit, 0, "duplicate identity");
            seen[slot] |= bit;
            let expected = if id == 0 {
                None
            } else {
                value(pattern, id - 1, n)
            };
            assert_eq!(row.logical, expected);
            assert_eq!(row.key.generation, GenerationId(1));
            if id == 0 {
                assert_eq!(row.kind, Kind::Directory);
                assert_eq!(row.path, root);
            } else {
                assert_eq!(row.kind, Kind::File);
                assert_eq!(row.path, root.join(format!("file-{:010}.txt", id - 1)));
            }
            if let Some((a, rank)) = previous {
                // Independent semantic comparator: ordinary numeric comparison,
                // explicit None handling and native rank on equality. No packed
                // sort-key or byte-complement implementation is reused.
                let ordering = match (a, row.logical) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(a), Some(b)) => {
                        if descending {
                            b.cmp(&a)
                        } else {
                            a.cmp(&b)
                        }
                    }
                };
                assert!(
                    ordering.is_lt() || (ordering.is_eq() && rank < id),
                    "incorrect global order"
                );
            }
            previous = Some((row.logical, id));
            count += 1;
        }
        if !page.more {
            break;
        }
        cursor = page.next;
    }
    assert_eq!(count, n + 1); // unique membership in 0..=n implies full coverage
    Ok(
        serde_json::json!({"rows":count,"descending":descending,"seconds":start.elapsed().as_secs_f64(),"max_page_ms":max_page_ms,"oracle_bitset_bytes":seen.len(),"exact":true}),
    )
}
fn run(
    dir: &Path,
    n: u64,
    pattern: &str,
    scratch: &Path,
    cancel_phase: Option<&str>,
) -> Result<()> {
    let scratch = tempfile::tempdir_in(scratch)?;
    let start = Instant::now();
    let preview = Unverified::open(dir, GenerationId(1))?;
    let root = preview.prefix(&Default::default())?.saved.root;
    let store = preview.verify(&Default::default())?;
    let certified_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut phases = Vec::new();
    let mut last = String::new();
    let mut event = |phase: &str, usage: sortsize_store::inventory::queries::TemporaryUsage| {
        if phase != last {
            let v = serde_json::json!({"phase":phase,"elapsed_ms":start.elapsed().as_secs_f64()*1000.,"temporary":usage,"peak_process_bytes":memory()});
            eprintln!("{v}");
            phases.push(v);
            last = phase.into();
        }
    };
    let mut q = store.start_query(
        GenerationId(1),
        &root,
        &Default::default(),
        scratch.path(),
        512 * 1024 * 1024,
    )?;
    loop {
        event(q.preparation_phase(), q.temporary_usage());
        if q.advance(&Default::default())? {
            break;
        }
    }
    event("query-ready", q.temporary_usage());
    assert_eq!(q.counts.matches, n + 1);
    let query_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut old = q.order_by_size(false, scratch.path(), 512 * 1024 * 1024)?;
    let mut max_step_ms = 0f64;
    loop {
        event(
            &format!("old-{}", old.preparation_phase()),
            old.temporary_usage(),
        );
        let t = Instant::now();
        let done = old.advance(&Default::default())?;
        max_step_ms = max_step_ms.max(t.elapsed().as_secs_f64() * 1000.);
        if done {
            break;
        }
    }
    event("old-ready", q.temporary_usage());
    let ascending_ms = start.elapsed().as_secs_f64() * 1000. - query_ms;
    let old_first = old
        .page(None, false)?
        .rows
        .iter()
        .map(|r| (r.key, r.logical, r.path.clone()))
        .collect::<Vec<_>>();
    let new_start = Instant::now();
    let mut new = q.order_by_size(true, scratch.path(), 512 * 1024 * 1024)?;
    let cancel = CancellationToken::default();
    let mut cancellation_ms = None;
    loop {
        let phase = new.preparation_phase();
        event(phase, new.temporary_usage());
        if cancel_phase == Some(phase) {
            cancel.cancel();
            let t = Instant::now();
            assert!(new.advance(&cancel).is_err());
            cancellation_ms = Some(t.elapsed().as_secs_f64() * 1000.);
            break;
        }
        let t = Instant::now();
        let done = new.advance(&cancel)?;
        max_step_ms = max_step_ms.max(t.elapsed().as_secs_f64() * 1000.);
        if done {
            break;
        }
    }
    let descending_ms = new_start.elapsed().as_secs_f64() * 1000.;
    event(
        if cancellation_ms.is_some() {
            "cancelled"
        } else {
            "order-ready"
        },
        q.temporary_usage(),
    );
    let order_ready_ms = start.elapsed().as_secs_f64() * 1000.;
    let peak_before_oracle = memory();
    drop(event);
    let new_oracle = if cancellation_ms.is_none() {
        Some(verify_pages(
            |c| new.page(c, false),
            &root,
            n,
            pattern,
            true,
        )?)
    } else {
        assert!(new.page(None, false).is_err());
        None
    };
    drop(new);
    assert_eq!(
        old_first,
        old.page(None, false)?
            .rows
            .iter()
            .map(|r| (r.key, r.logical, r.path.clone()))
            .collect::<Vec<_>>()
    );
    let old_oracle = verify_pages(|c| old.page(c, false), &root, n, pattern, false)?;
    println!(
        "{}",
        serde_json::json!({"status":if cancellation_ms.is_some(){"CANCELLED_PRESERVED"}else{"EXACT"},"files":n,"pattern":pattern,"quota_mib":512,"certified_ms":certified_ms,"query_ready_ms":query_ms,"ascending_ms":ascending_ms,"descending_ms":descending_ms,"first_two_orders_ms":order_ready_ms,"max_advance_ms":max_step_ms,"cancellation_ms":cancellation_ms,"cancel_phase":cancel_phase,"temporary":q.temporary_usage(),"peak_process_before_oracle":peak_before_oracle,"peak_process_bytes":memory(),"ascending_oracle":old_oracle,"descending_oracle":new_oracle,"previous_order_preserved":true,"query_complete":q.complete(),"source_coverage":format!("{:?}",q.source_coverage),"phases":phases,"synthetic":true,"source_acquisition":false})
    );
    Ok(())
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let action = args.next().ok_or("prepare|run")?;
    let dir = PathBuf::from(args.next().ok_or("private fixture store")?);
    let n = args.next().ok_or("file count")?.parse::<u64>()?;
    let pattern = args.next().ok_or("pattern")?;
    if !support::pattern_ok(&pattern) || n == 0 {
        return Err("invalid fixture shape".into());
    }
    Governor::global().configure(MemoryMode::Auto, sortsize_core::resources::native_memory());
    match action.as_str() {
        "prepare" => prepare(&dir, n, &pattern),
        "run" => {
            let scratch = PathBuf::from(args.next().ok_or("private scratch")?);
            let phase = args.next();
            if phase
                .as_ref()
                .is_some_and(|s| !matches!(s.as_str(), "segments" | "merge" | "publication"))
            {
                return Err("invalid cancellation phase".into());
            }
            run(&dir, n, &pattern, &scratch, phase.as_deref())
        }
        _ => Err("prepare|run".into()),
    }
}
