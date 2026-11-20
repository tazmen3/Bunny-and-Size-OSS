//! Synthetic store benchmark, never scans a user source. One process per case.
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sortsize_store::{
    Durability, QUALIFICATION_SCHEMA,
    codec::{NativeName, unsigned},
    open,
};
use std::{
    error::Error,
    path::Path,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
fn bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
fn sizes(dir: &Path) -> Value {
    json!({"db":bytes(&dir.join("inventory.db")),"wal":bytes(&dir.join("inventory.db-wal")),"shm":bytes(&dir.join("inventory.db-shm")),"migration":bytes(&dir.join("migration.db")),"migration_wal":bytes(&dir.join("migration.db-wal")),"migration_shm":bytes(&dir.join("migration.db-shm"))})
}
fn total(v: &Value) -> u64 {
    v.as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum()
}
#[cfg(target_os = "linux")]
fn resources() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = status
        .lines()
        .find_map(|s| s.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024;
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    let mut v = json!({"peak_rss_bytes":peak});
    for line in io.lines() {
        let (k, val) = line.split_once(':').unwrap();
        v[k] = json!(val.trim().parse::<u64>().unwrap());
    }
    v
}
#[cfg(windows)]
fn resources() -> Value {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{GetCurrentProcess, GetProcessIoCounters, IO_COUNTERS},
    };
    // SAFETY: correctly sized initialized output structures and current-process pseudo-handle.
    unsafe {
        let mut mem: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let mut io: IO_COUNTERS = std::mem::zeroed();
        assert_ne!(
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut mem,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32
            ),
            0
        );
        assert_ne!(GetProcessIoCounters(GetCurrentProcess(), &mut io), 0);
        json!({"peak_rss_bytes":mem.PeakWorkingSetSize,"read_transfer_bytes":io.ReadTransferCount,"write_transfer_bytes":io.WriteTransferCount,"read_operations":io.ReadOperationCount,"write_operations":io.WriteOperationCount})
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
fn resources() -> Value {
    json!({"unavailable":true})
}

fn name(i: u64) -> Vec<u8> {
    let text = format!("entry-{i:016x}-{}", "x".repeat((i % 41) as usize));
    if i.is_multiple_of(2) {
        let mut v = text.into_bytes();
        v.push(0xff);
        NativeName::Posix(v).encode()
    } else {
        let mut v: Vec<_> = text.encode_utf16().collect();
        v.push(0xd800);
        NativeName::Windows(v).encode()
    }
}
fn page(db: &Connection, g: u64) -> Result<Vec<Vec<u8>>> {
    Ok(db
        .prepare(
            "SELECT id FROM entries WHERE generation=?1 AND parent=?2 ORDER BY name LIMIT 200",
        )?
        .query_map(params![unsigned(g), unsigned(0)], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err(
            "usage: sqlite_qualification NEW_DIRECTORY ROWS NORMAL|FULL GENERATIONS".into(),
        );
    }
    let dir = Path::new(&args[1]);
    std::fs::create_dir(dir)?; // Never overwrite an earlier case.
    let count = args[2].parse::<u64>()?;
    let generations = args[4].parse::<u64>()?;
    if count == 0 || !(2..=3).contains(&generations) {
        return Err("invalid case".into());
    }
    let durability = match args[3].as_str() {
        "NORMAL" => Durability::Reconstructible,
        "FULL" => Durability::Retained,
        _ => return Err("invalid durability".into()),
    };
    let path = dir.join("inventory.db");
    let start = Instant::now();
    let mut db = open(&path, durability)?;
    let open_ms = ms(start);
    let schema_start = Instant::now();
    {
        let tx = db.transaction()?;
        tx.execute_batch(QUALIFICATION_SCHEMA)?;
        tx.commit()?;
    }
    let schema_ms = ms(schema_start);
    let options: Vec<String> = db
        .prepare("PRAGMA compile_options")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut commits = Vec::new();
    let mut ingest = Vec::new();
    let mut first_page_ms = None;
    let mut peak_disk = 0;
    let mut peak_wal = 0;
    for g in 0..generations {
        let started = Instant::now();
        for lo in (0..count).step_by(512) {
            let tx = db.transaction()?;
            if lo == 0 {
                tx.execute("INSERT INTO generations VALUES(?1,0,0,0)", [unsigned(g)])?;
            }
            {
                let mut entry =
                    tx.prepare("INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,0)")?;
                let mut contribution =
                    tx.prepare("INSERT INTO contributions VALUES(?1,?2,?3,?4,?5)")?;
                let mut task=tx.prepare("INSERT INTO tasks VALUES(?1,?2,?3,?3,?4,0,NULL) ON CONFLICT(generation,directory) DO UPDATE SET ack=excluded.ack")?;
                for i in lo..(lo + 512).min(count) {
                    let key = unsigned(i);
                    let parent = unsigned(i / 1024);
                    let generation = unsigned(g);
                    let size = unsigned(if i == count - 1 {
                        u64::MAX
                    } else {
                        i.wrapping_mul(104729)
                    });
                    let allocated = if i.is_multiple_of(17) {
                        None
                    } else {
                        Some(size)
                    };
                    entry.execute(params![
                        generation,
                        key,
                        parent,
                        name(i),
                        i64::from(i.is_multiple_of(1024)),
                        size,
                        allocated,
                        -(i as i64),
                        (i % 1_000_000_000) as i64,
                        unsigned(1)
                    ])?;
                    contribution.execute(params![generation, key, parent, size, unsigned(1)])?;
                    if i.is_multiple_of(1024) || i == (lo + 512).min(count) - 1 {
                        task.execute(params![generation, parent, unsigned(1), i as i64])?;
                    }
                }
            }
            let commit = Instant::now();
            tx.commit()?;
            commits.push(ms(commit));
            if first_page_ms.is_none() {
                assert!(!page(&db, g)?.is_empty());
                first_page_ms = Some(ms(start));
            }
            let s = sizes(dir);
            peak_disk = peak_disk.max(total(&s));
            peak_wal = peak_wal.max(s["wal"].as_u64().unwrap());
        }
        ingest.push(json!({"generation":g,"milliseconds":ms(started),"rows_per_second":count as f64/started.elapsed().as_secs_f64()}));
        db.execute(
            "UPDATE generations SET sealed=1,coverage=1,revision=revision+1 WHERE id=?1",
            [unsigned(g)],
        )?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM entries WHERE generation=?1",
                [unsigned(g)],
                |r| r.get::<_, i64>(0)
            )?,
            count as i64
        );
    }
    let mut reader = open(&path, durability)?;
    let mut page_times = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        assert_eq!(page(&reader, 0)?.len(), 200.min(count as usize));
        page_times.push(ms(t));
    }
    let mut plans = Vec::new();
    for sql in [
        "SELECT id FROM entries WHERE generation=?1 AND parent=?2 ORDER BY name LIMIT 200",
        "SELECT id FROM entries WHERE generation=?1 AND logical>?2 ORDER BY logical,id LIMIT 200",
    ] {
        let plan: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map(params![unsigned(0), unsigned(0)], |r| r.get(3))?
            .collect::<std::result::Result<_, _>>()?;
        assert!(
            plan.iter()
                .all(|p| !p.contains("TEMP B-TREE") && !p.starts_with("SCAN "))
        );
        plans.push(json!({"sql":sql,"plan":plan}));
    }
    // SQL -> Rust native-unit oracle for one complete 1024-child directory.
    let sql_names: Vec<Vec<u8>> = db
        .prepare("SELECT name FROM entries WHERE generation=?1 AND parent=?2 ORDER BY name")?
        .query_map(params![unsigned(0), unsigned(0)], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut oracle: Vec<_> = (0..count.min(1024))
        .map(|i| NativeName::decode(&name(i)).unwrap())
        .collect();
    oracle.sort();
    assert_eq!(
        sql_names,
        oracle.iter().map(NativeName::encode).collect::<Vec<_>>()
    );
    assert_eq!(
        db.query_row(
            "SELECT logical FROM entries WHERE generation=?1 AND id=?2",
            params![unsigned(0), unsigned(count - 1)],
            |r| r.get::<_, Vec<u8>>(0)
        )?,
        unsigned(u64::MAX)
    );
    let cancel = Instant::now();
    let interrupt = reader.get_interrupt_handle();
    reader.progress_handler(
        100,
        Some(move || {
            interrupt.interrupt();
            false
        }),
    )?;
    let error=reader.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<1000000000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0)).unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    );
    let cancel_ms = ms(cancel);
    reader.progress_handler(0, None::<fn() -> bool>)?;
    assert!(!page(&reader, 0)?.is_empty());
    let rollback = Instant::now();
    {
        let tx = db.transaction()?;
        tx.execute("UPDATE generations SET coverage=0", [])?;
    }
    let rollback_ms = ms(rollback);
    assert_eq!(
        db.query_row("SELECT min(coverage) FROM generations", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    let checkpoint = Instant::now();
    let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    assert_eq!(busy, 0);
    let checkpoint_ms = ms(checkpoint);
    let retained_sizes = sizes(dir);
    // Force external-capable sort, materialize on disk, quantify logical temp pages.
    let sort = Instant::now();
    db.execute("CREATE TEMP TABLE ordered AS SELECT id,name FROM entries WHERE generation=?1 ORDER BY substr(name,3),id",[unsigned(0)])?;
    let sort_ms = ms(sort);
    let temp_bytes = db.query_row("PRAGMA temp.page_count", [], |r| r.get::<_, i64>(0))?
        * db.query_row("PRAGMA temp.page_size", [], |r| r.get::<_, i64>(0))?;
    db.execute_batch("DROP TABLE temp.ordered")?;
    let migration = Instant::now();
    let mut copy = open(&dir.join("migration.db"), Durability::Retained)?;
    {
        let backup = rusqlite::backup::Backup::new(&db, &mut copy)?;
        backup.run_to_completion(256, Duration::ZERO, None)?;
    }
    copy.execute_batch("BEGIN IMMEDIATE; ALTER TABLE generations ADD COLUMN migrated INTEGER NOT NULL DEFAULT 0; PRAGMA user_version=2; COMMIT")?;
    assert_eq!(
        copy.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        copy.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))?,
        (count * generations) as i64
    );
    let migration_ms = ms(migration);
    let migration_sizes = sizes(dir);
    peak_disk = peak_disk.max(total(&migration_sizes) + temp_bytes as u64);
    commits.sort_by(f64::total_cmp);
    page_times.sort_by(f64::total_cmp);
    let result = json!({"rows_per_generation":count,"generations":generations,"durability":args[3],"sqlite":rusqlite::version(),"source_id":db.query_row("SELECT sqlite_source_id()",[],|r|r.get::<_,String>(0))?,"compile_options":options,"ingestion":ingest,"open_ms":open_ms,"schema_ms":schema_ms,"first_page_from_open_ms":first_page_ms,"page_p95_ms":page_times[28],"commit_p95_ms":commits[commits.len()*95/100],"commit_max_ms":commits.last(),"cancel_ms":cancel_ms,"rollback_ms":rollback_ms,"checkpoint_ms":checkpoint_ms,"sort_ms":sort_ms,"temp_logical_bytes":temp_bytes,"migration_ms":migration_ms,"retained_files":retained_sizes,"migration_files":migration_sizes,"sampled_peak_owned_bytes_plus_temp":peak_disk,"sampled_peak_wal_bytes":peak_wal,"resources":resources(),"plans":plans,"exactness":"PASS","total_ms":ms(start)});
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
