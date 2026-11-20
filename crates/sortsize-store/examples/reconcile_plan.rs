//! Diagnostic local, sur une copie clôturée uniquement. Aucune écriture SQLite.
//! Usage: cargo run -p sortsize-store --example reconcile_plan -- <copie.db>
//! Compare les balayages de réconciliation et de LivePreview avec l'index
//! parent/id explicite, y compris à EOF et pour un parent absent. Les compteurs
//! VM révèlent aussi les parcours par clé primaire que FULLSCAN_STEP ne compte
//! pas. Les durées dépendent du cache et ne constituent pas un benchmark.
use rusqlite::{Connection, OpenFlags, StatementStatus, ToSql, params, types::Value};
use std::{error::Error, path::Path, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Rows = Vec<Vec<Value>>;

fn probe(db: &Connection, label: &str, sql: &str, values: &[&dyn ToSql]) -> Result<Rows> {
    let mut plan = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    for detail in plan.query_map(values, |row| row.get::<_, String>(3))? {
        println!("plan {label}: {}", detail?);
    }
    let mut statement = db.prepare(sql)?;
    let columns = statement.column_count();
    let start = Instant::now();
    let mut result = Vec::new();
    {
        let mut rows = statement.query(values)?;
        while let Some(row) = rows.next()? {
            if result.len() >= 512 {
                return Err("diagnostic result bound exceeded".into());
            }
            result.push(
                (0..columns)
                    .map(|column| row.get::<_, Value>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
    }
    println!(
        "statement {label}: rows={} vm_steps={} fullscan_steps={} sorts={} elapsed_ns={}",
        result.len(),
        statement.get_status(StatementStatus::VmStep),
        statement.get_status(StatementStatus::FullscanStep),
        statement.get_status(StatementStatus::Sort),
        start.elapsed().as_nanos(),
    );
    Ok(result)
}

fn immutable_uri(path: &Path) -> Result<String> {
    let absolute = path.canonicalize()?;
    let text = absolute
        .to_str()
        .ok_or("diagnostic requires a UTF-8 path")?;
    #[cfg(windows)]
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let text = text.replace('\\', "/");
    if text.starts_with("UNC/") || text.starts_with("//") {
        return Err("diagnostic requires a local copied database".into());
    }
    let mut uri = String::from("file:");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/:._-".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut uri, "%{byte:02X}")?;
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or("usage: reconcile_plan <closed-copy.db>")?;
    if args.next().is_some() {
        return Err("expected exactly one database path".into());
    }
    let path = Path::new(&path);
    for suffix in ["-wal", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        if Path::new(&sidecar).exists() {
            return Err("database has a WAL/journal; use a closed checkpointed copy".into());
        }
    }
    let db = Connection::open_with_flags(
        immutable_uri(path)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    println!("sqlite_version={}", rusqlite::version());
    let (generation, context, sealed): (Vec<u8>, Vec<u8>, bool) = db.query_row(
        "SELECT id,context,sealed FROM generations ORDER BY id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if !sealed {
        return Err("expected a sealed inventory copy".into());
    }
    let mut counts = db.prepare("SELECT kind,count(*) FROM tasks GROUP BY kind ORDER BY kind")?;
    for count in counts.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))? {
        let (kind, count) = count?;
        println!("tasks kind={kind} count={count}");
    }
    let directory: Vec<u8> = db.query_row(
        "SELECT directory FROM tasks WHERE generation=?1 AND kind=0 AND directory>zeroblob(8) ORDER BY directory LIMIT 1",
        [&generation],
        |row| row.get(0),
    )?;
    let (children, files): (i64, i64) = db.query_row(
        "SELECT count(*),coalesce(sum(kind=1),0) FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2",
        params![generation, directory],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    println!("selected_directory children={children} files={files}");
    if children != 1000 || files != 1000 {
        return Err("expected first non-root directory to contain exactly 1000 files".into());
    }
    probe(
        &db,
        "reconcile_next",
        "SELECT directory,attempt,owner,branch FROM tasks WHERE generation=?1 AND kind=0 AND listing=1 AND reconciled=0 AND substr(key,1,length(?2))=?2 ORDER BY directory LIMIT 1",
        &[&generation, &context],
    )?;
    let normal = "SELECT id,attempt FROM entries WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT ?4";
    let forced = "SELECT id,attempt FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT ?4";
    let preview = "SELECT id,kind,logical FROM entries WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT 1";
    let preview_forced = "SELECT id,kind,logical FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND id>?3 ORDER BY id LIMIT 1";
    let tree = "SELECT id,kind,substr(name,1,32769) FROM entries WHERE generation=?1 AND parent=?2 AND present=1 AND id>?3 ORDER BY id LIMIT 256";
    let tree_forced = "SELECT id,kind,substr(name,1,32769) FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND present=1 AND id>?3 ORDER BY id LIMIT 256";
    let tree_kinds = "SELECT kind FROM entries WHERE generation=?1 AND parent=?2 AND present=1 ORDER BY id LIMIT 257";
    let tree_kinds_forced = "SELECT kind FROM entries INDEXED BY entries_parent_id WHERE generation=?1 AND parent=?2 AND present=1 ORDER BY id LIMIT 257";
    let mut after = vec![0u8; 8];
    for offset in [0, 512] {
        let preview_values: &[&dyn ToSql] = &[&generation, &directory, &after];
        let tree_actual = probe(
            &db,
            &format!("tree_normal_after_{offset}"),
            tree,
            preview_values,
        )?;
        let tree_indexed = probe(
            &db,
            &format!("tree_indexed_after_{offset}"),
            tree_forced,
            preview_values,
        )?;
        if tree_actual != tree_indexed {
            return Err("tree normal and indexed queries disagree".into());
        }
        let preview_actual = probe(
            &db,
            &format!("preview_normal_after_{offset}"),
            preview,
            preview_values,
        )?;
        let preview_indexed = probe(
            &db,
            &format!("preview_indexed_after_{offset}"),
            preview_forced,
            preview_values,
        )?;
        if preview_actual != preview_indexed {
            return Err("preview normal and indexed queries disagree".into());
        }
        let values: &[&dyn ToSql] = &[&generation, &directory, &after, &512i64];
        let actual = probe(&db, &format!("child_normal_after_{offset}"), normal, values)?;
        let indexed = probe(
            &db,
            &format!("child_indexed_after_{offset}"),
            forced,
            values,
        )?;
        if actual != indexed {
            return Err("normal and indexed queries disagree".into());
        }
        let Some(Value::Blob(last_id)) = actual.last().and_then(|row| row.first()) else {
            return Err("missing child cursor".into());
        };
        after = last_id.clone();
    }
    for (label, parent, cursor) in [
        ("eof", directory, after),
        ("absent", u64::MAX.to_be_bytes().to_vec(), vec![0u8; 8]),
    ] {
        let values: &[&dyn ToSql] = &[&generation, &parent, &cursor];
        let tree_actual = probe(&db, &format!("tree_normal_{label}"), tree, values)?;
        let tree_indexed = probe(&db, &format!("tree_indexed_{label}"), tree_forced, values)?;
        if tree_actual != tree_indexed || !tree_actual.is_empty() {
            return Err("expected equal empty tree results".into());
        }
        let actual = probe(&db, &format!("preview_normal_{label}"), preview, values)?;
        let indexed = probe(
            &db,
            &format!("preview_indexed_{label}"),
            preview_forced,
            values,
        )?;
        if actual != indexed || !actual.is_empty() {
            return Err("expected equal empty preview results".into());
        }
        let values: &[&dyn ToSql] = &[&generation, &parent, &cursor, &512i64];
        let actual = probe(&db, &format!("child_normal_{label}"), normal, values)?;
        let indexed = probe(&db, &format!("child_indexed_{label}"), forced, values)?;
        if actual != indexed || !actual.is_empty() {
            return Err("expected equal empty sweep results".into());
        }
        let values: &[&dyn ToSql] = &[&generation, &parent];
        let actual = probe(
            &db,
            &format!("tree_kinds_normal_{label}"),
            tree_kinds,
            values,
        )?;
        let indexed = probe(
            &db,
            &format!("tree_kinds_indexed_{label}"),
            tree_kinds_forced,
            values,
        )?;
        if actual != indexed {
            return Err("tree kind normal and indexed queries disagree".into());
        }
    }
    println!("result_equality=true; elapsed values are diagnostic, not a benchmark");
    Ok(())
}
