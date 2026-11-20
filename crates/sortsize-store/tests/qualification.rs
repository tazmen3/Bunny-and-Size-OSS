use rusqlite::{Connection, ErrorCode, params};
use sortsize_store::{Durability, QUALIFICATION_SCHEMA, codec::*, open};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn native_and_unsigned_sql_order_match_rust_without_loss() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE keys(k BLOB PRIMARY KEY) WITHOUT ROWID")
        .unwrap();
    let mut names = vec![
        NativeName::Posix(vec![]),
        NativeName::Posix(vec![0xff, 0, 0xfe]),
        NativeName::Windows(vec![0xd800]),
        NativeName::Windows(vec![0xdc00]),
        NativeName::Windows(vec![0xd800, 0xdc00]),
        NativeName::Windows(vec![0xe000]),
        NativeName::Windows(vec![0xff, 0x100]),
        NativeName::Opaque(vec![0xff]),
    ];
    for i in 0u16..4096 {
        names.push(NativeName::Windows(vec![i.wrapping_mul(17), i, 0xdfff]));
        names.push(NativeName::Posix(i.to_be_bytes().to_vec()));
    }
    names.sort();
    names.dedup();
    for name in names.iter().rev() {
        assert_eq!(NativeName::decode(&name.encode()), Ok(name.clone()));
        db.execute("INSERT INTO keys VALUES(?1)", [name.encode()])
            .unwrap();
    }
    let got: Vec<_> = db
        .prepare("SELECT k FROM keys ORDER BY k")
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|x| NativeName::decode(&x.unwrap()).unwrap())
        .collect();
    assert_eq!(got, names); // Explicit Rust native-unit ordering, not locale/case folding.
    db.execute("DELETE FROM keys", []).unwrap();
    let numbers = [
        0,
        1,
        255,
        256,
        i64::MAX as u64,
        1u64 << 63,
        u64::MAX - 1,
        u64::MAX,
    ];
    for n in numbers.iter().rev() {
        db.execute("INSERT INTO keys VALUES(?1)", [unsigned(*n)])
            .unwrap();
    }
    let got: Vec<_> = db
        .prepare("SELECT k FROM keys ORDER BY k")
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|x| decode_unsigned(&x.unwrap()).unwrap())
        .collect();
    assert_eq!(got, numbers);
    assert!(NativeName::decode(&[1, 1, 0]).is_err());
    assert!(NativeName::decode(&[2, 0]).is_err());
    assert!(decode_unsigned(&[0; 7]).is_err());
}

#[test]
fn negative_timestamp_precision_and_null_are_exact() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE dates(s INTEGER,ns INTEGER,bytes BLOB) STRICT")
        .unwrap();
    for (s, ns) in [
        (i64::MIN, 0),
        (-1, 999_999_999),
        (0, 0),
        (i64::MAX, 999_999_999),
    ] {
        let (s, ns) = timestamp(s, ns).unwrap();
        db.execute("INSERT INTO dates VALUES(?1,?2,NULL)", params![s, ns])
            .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT s,ns,bytes FROM dates ORDER BY rowid DESC LIMIT 1",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?
                ))
            )
            .unwrap(),
            (s, ns, None)
        );
    }
    assert!(timestamp(0, 1_000_000_000).is_err());
    assert_eq!(u64::MAX.checked_add(1), None);
}

#[test]
fn rollback_and_short_reader_release_wal() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");
    let mut writer = open(&path, Durability::Retained).unwrap();
    writer
        .execute_batch(
            "CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(1); PRAGMA wal_checkpoint(TRUNCATE)",
        )
        .unwrap();
    {
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO t VALUES(2)", []).unwrap();
    }
    assert_eq!(
        writer
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let reader = open(&path, Durability::Retained).unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM t").unwrap();
    writer.execute("INSERT INTO t VALUES(3)", []).unwrap();
    let pinned: (i64, i64, i64) = writer
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(pinned.0, 1);
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    reader.execute_batch("COMMIT").unwrap();
    let released: i64 = writer
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(released, 0);
}

#[test]
fn cancellation_rolls_back_only_target_connection_and_next_job_survives() {
    let mut db = Connection::open_in_memory().unwrap();
    let other = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(7); BEGIN")
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    db.progress_handler(100, Some(move || flag.load(Ordering::Relaxed)))
        .unwrap();
    cancel.store(true, Ordering::Relaxed);
    let err=db.execute("INSERT INTO t WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<1000000) SELECT n FROM x",[]).unwrap_err();
    assert_eq!(
        err.sqlite_error_code(),
        Some(ErrorCode::OperationInterrupted)
    );
    cancel.store(false, Ordering::Relaxed);
    db.progress_handler(0, None::<fn() -> bool>).unwrap();
    assert!(db.is_autocommit());
    assert_eq!(
        db.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        other
            .query_row("SELECT 42", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        42
    );
    assert_eq!(
        db.query_row("SELECT 43", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        43
    );
}

#[test]
fn native_interrupt_handle_targets_running_work() {
    let mut db = Connection::open_in_memory().unwrap();
    let handle = db.get_interrupt_handle();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let mut first = true;
    db.progress_handler(
        100,
        Some(move || {
            if first {
                first = false;
                started_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            }
            false
        }),
    )
    .unwrap();
    let worker = std::thread::spawn(move || {
        let err=db.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<100000000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0)).unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(ErrorCode::OperationInterrupted)
        );
        db.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(
            db.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    handle.interrupt();
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn disk_full_rolls_back_and_schema_preserves_unsigned_fields() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = open(&temp.path().join("full.db"), Durability::Retained).unwrap();
    db.execute_batch(QUALIFICATION_SCHEMA).unwrap();
    db.execute(
        "INSERT INTO generations VALUES(?1,0,0,0)",
        [unsigned(u64::MAX)],
    )
    .unwrap();
    let pages: i64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
    db.pragma_update(None, "max_page_count", pages + 2).unwrap();
    let tx = db.transaction().unwrap();
    let err = tx
        .execute(
            "INSERT INTO entries VALUES(?1,?2,?2,zeroblob(1000000),0,?2,NULL,-1,999999999,?2,1,0)",
            params![unsigned(u64::MAX), unsigned(0)],
        )
        .unwrap_err();
    assert_eq!(err.sqlite_error_code(), Some(ErrorCode::DiskFull));
    drop(tx);
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn bundled_version_and_effective_configuration() {
    assert_eq!(rusqlite::version(), "3.54.1");
    for (mode, expected) in [(Durability::Retained, 2), (Durability::Reconstructible, 1)] {
        let t = tempfile::tempdir().unwrap();
        let db = open(&t.path().join("db"), mode).unwrap();
        assert_eq!(
            db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            expected
        );
        assert_eq!(
            db.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            db.query_row("PRAGMA mmap_size", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn native_os_strings_round_trip_without_unicode_replacement() {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let native = std::ffi::OsString::from_vec(vec![b'a', 0xff, 0xfe]);
        let encoded = NativeName::Posix(native.as_bytes().to_vec()).encode();
        let NativeName::Posix(raw) = NativeName::decode(&encoded).unwrap() else {
            panic!()
        };
        assert_eq!(std::ffi::OsString::from_vec(raw), native);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let native = std::ffi::OsString::from_wide(&[0x61, 0xd800, 0x62, 0xdc00]);
        let encoded = NativeName::Windows(native.encode_wide().collect()).encode();
        let NativeName::Windows(raw) = NativeName::decode(&encoded).unwrap() else {
            panic!()
        };
        assert_eq!(std::ffi::OsString::from_wide(&raw), native);
    }
}
