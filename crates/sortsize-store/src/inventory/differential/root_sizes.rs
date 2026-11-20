//! Bounded rankings at the first level of a saved measurement.
use super::*;
use crate::inventory::folder_totals::FolderFold;
use std::{cmp::Ordering, ffi::OsString};

const ROOT: EntryId = EntryId(0);
const SIZE_LIMIT: usize = 8;
const CHANGE_LIMIT: usize = 5;
const NAME_LIMIT: usize = 32 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct RootSize {
    pub name: OsString,
    pub is_folder: bool,
    pub logical: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RootChange {
    pub name: OsString,
    pub delta: i128,
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Contract("root measurement projection cancelled"));
    }
    Ok(())
}

fn name(raw: &[u8]) -> Result<OsString> {
    if raw.len() > NAME_LIMIT {
        return Err(Error::Contract(
            "root measurement name exceeds projection budget",
        ));
    }
    let decoded =
        NativeName::decode(raw).map_err(|_| Error::Contract("invalid root measurement name"))?;
    crate::scheduler::native_name(&decoded)
}

fn fold(lease: &Lease<'_>, scratch: &Path, cancel: &CancellationToken) -> Result<FolderFold> {
    let mut fold = FolderFold::default();
    loop {
        check_cancelled(cancel)?;
        if lease.store.folder_fold_step_at(
            lease.measurement.context.generation,
            &mut fold,
            scratch,
        )? {
            return Ok(fold);
        }
    }
}

fn root_items(
    lease: &Lease<'_>,
    cancel: &CancellationToken,
    mut visit: impl FnMut(&Item) -> Result<()>,
) -> Result<()> {
    let generation = lease.measurement.context.generation;
    let root = directory(lease.store, generation, Some(ROOT.0), true)?;
    if !root.current {
        if lease.measurement.coverage == ScopeCoverage::Complete {
            return Err(Error::Contract(
                "complete root measurement has no current listing",
            ));
        }
        return Ok(());
    }
    let mut cursor = Vec::new();
    loop {
        check_cancelled(cancel)?;
        let batch = items(lease.store, generation, Some(ROOT.0), &cursor)?;
        for item in &batch.rows {
            if item.known && Some(item.attempt) == root.attempt {
                visit(item)?;
            }
        }
        if !batch.more {
            return check_cancelled(cancel);
        }
        cursor.clone_from(&batch.rows.last().expect("nonempty diff batch").name);
    }
}

fn rank<T>(rows: &mut Vec<T>, row: T, limit: usize, mut order: impl FnMut(&T, &T) -> Ordering) {
    let index = rows.partition_point(|existing| order(existing, &row).is_le());
    if index < limit {
        if rows.len() == limit {
            rows.pop();
        }
        rows.insert(index, row);
    }
}

/// Largest known logical sizes under the measured root. Folder sizes include
/// their descendants. A partial measurement remains partial.
pub fn largest_root_entries(
    lease: &Lease<'_>,
    scratch: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<RootSize>> {
    let _memory = Governor::global()
        .reserve(Category::Queries, 1024 * 1024, None)
        .ok_or(Error::Contract("root measurement projection memory budget"))?;
    let fold = fold(lease, scratch, cancel)?;
    let generation = lease.measurement.context.generation;
    let mut sizes = Vec::with_capacity(SIZE_LIMIT);
    root_items(lease, cancel, |item| {
        let is_folder = item.kind == Kind::Directory as i64;
        let logical = match item.kind {
            kind if kind == Kind::File as i64 => item.logical,
            kind if kind == Kind::Directory as i64 => lease
                .store
                .folder_measure(generation, EntryId(item.id), &fold)?
                .and_then(|measure| measure.logical),
            _ => None,
        };
        if let Some(logical) = logical {
            let row = RootSize {
                name: name(&item.name)?,
                is_folder,
                logical,
            };
            rank(&mut sizes, row, SIZE_LIMIT, |a, b| {
                b.logical.cmp(&a.logical).then_with(|| a.name.cmp(&b.name))
            });
        }
        Ok(())
    })?;
    Ok(sizes)
}

fn record_folders(
    db: &Connection,
    lease: &Lease<'_>,
    scratch: &Path,
    cancel: &CancellationToken,
    statement: &str,
) -> Result<()> {
    let generation = lease.measurement.context.generation;
    let fold = fold(lease, scratch, cancel)?;
    let mut insert = db.prepare(statement)?;
    root_items(lease, cancel, |item| {
        if item.kind != Kind::Directory as i64 {
            return Ok(());
        }
        if item.name.len() > NAME_LIMIT {
            return Err(Error::Contract(
                "root measurement name exceeds projection budget",
            ));
        }
        let measure = lease
            .store
            .folder_measure(generation, EntryId(item.id), &fold)?
            .filter(|measure| measure.unknown == 0)
            .and_then(|measure| measure.logical)
            .ok_or(Error::Contract(
                "complete folder measurement is unavailable",
            ))?;
        insert.execute(params![item.name, unsigned(measure)])?;
        Ok(())
    })
}

impl Diff<'_> {
    /// Largest changes in immediate child folders, matched by recorded native
    /// name. This excludes root files and requires two complete comparable scopes.
    pub fn root_folder_changes(
        &self,
        scratch: &Path,
        cancel: &CancellationToken,
    ) -> Result<Vec<RootChange>> {
        if !self.summary.finished || self.summary.comparable_delta.is_none() {
            return Ok(Vec::new());
        }
        check_cancelled(cancel)?;
        // One Store admits one folder fold at a time. Stage the root rows on
        // the existing quota-controlled diff connection so this also works
        // for two generations leased from the same Store. No result rows are
        // retained past this call, even when a fold or write fails.
        let transaction = self.db.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE root_sizes(name BLOB PRIMARY KEY,old BLOB,new BLOB) WITHOUT ROWID;",
        )?;
        record_folders(
            &transaction,
            &self.old,
            scratch,
            cancel,
            "INSERT INTO root_sizes(name,old) VALUES(?1,?2)",
        )?;
        record_folders(
            &transaction,
            &self.new,
            scratch,
            cancel,
            "INSERT INTO root_sizes(name,new) VALUES(?1,?2)
             ON CONFLICT(name) DO UPDATE SET new=excluded.new",
        )?;
        let mut rises = Vec::with_capacity(CHANGE_LIMIT);
        let mut falls = Vec::with_capacity(CHANGE_LIMIT);
        {
            let mut select = transaction.prepare("SELECT name,old,new FROM root_sizes")?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                check_cancelled(cancel)?;
                let old = opt_number(row, 1)?.unwrap_or(0);
                let new = opt_number(row, 2)?.unwrap_or(0);
                if old == new {
                    continue;
                }
                let change = RootChange {
                    name: name(&row.get::<_, Vec<u8>>(0)?)?,
                    delta: i128::from(new) - i128::from(old),
                };
                let target = if change.delta > 0 {
                    &mut rises
                } else {
                    &mut falls
                };
                rank(target, change, CHANGE_LIMIT, |a, b| {
                    b.delta
                        .abs()
                        .cmp(&a.delta.abs())
                        .then_with(|| a.name.cmp(&b.name))
                });
            }
        }
        transaction.rollback()?;
        rises.extend(falls);
        Ok(rises)
    }
}
