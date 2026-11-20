//! Strict folder equality derived from the leased inventories and exact file
//! groups. All tree state and expansion rows belong to the existing quota VFS.
//! A hash only selects a pair: native names, kinds, exact groups, independent
//! identities, coverage and current proofs are checked again before publication.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct FolderGroup {
    pub id: u64,
    pub a: Row,
    pub b: Row,
    /// Optional third independently verified location from this same job.
    pub c: Option<Row>,
    pub a_side: u8,
    pub b_side: u8,
    pub c_side: Option<u8>,
    pub files: u64,
    pub logical_bytes: u64,
    pub verified: bool,
}
#[derive(Debug, Clone)]
pub struct FolderCursor {
    owner: PathBuf,
    group: u64,
    seq: i64,
}
#[derive(Debug, Clone)]
pub struct FolderDetail {
    pub a: Row,
    pub b: Row,
    /// Optional third independently verified location from this same job.
    pub c: Option<Row>,
    pub a_side: u8,
    pub b_side: u8,
    pub c_side: Option<u8>,
}
#[derive(Debug)]
pub struct FolderPage {
    pub rows: Vec<FolderDetail>,
    pub first: Option<FolderCursor>,
    pub last: Option<FolderCursor>,
    pub more: bool,
}
type StoredNode = (usize, u64, Vec<u8>, i64, Option<u64>);
type PairChild = (i64, Vec<u8>, i64, Option<Vec<u8>>, Option<Vec<u8>>);
type FreshRecord = (
    usize,
    Vec<u8>,
    i64,
    Option<i64>,
    Option<u32>,
    Option<Vec<u8>>,
);
type FileRevalidationRecord = (u64, Vec<u8>, usize, Vec<u8>, u64, u64, u64);

fn ready(job: &DuplicateJob<'_, '_>) -> Result<bool> {
    let exists: bool = job.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='folder_state')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    Ok(job.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM folder_state WHERE ready=1)",
        [],
        |r| r.get(0),
    )?)
}
pub(super) fn prepare(job: &mut DuplicateJob<'_, '_>, cancel: &CancellationToken) -> Result<()> {
    if let Err(error) = prepare_inner(job, cancel) {
        job.summary.folder_unverified = job.summary.folder_unverified.saturating_add(1);
        job.summary.folder_diagnostic = Some(error.to_string().chars().take(512).collect());
        // Release the optional tree projection so exact file grouping can use
        // its pages within the unchanged shared disk budget.
        let _ = job
            .db
            .execute_batch("DELETE FROM folder_nodes;DELETE FROM folder_state;");
    }
    Ok(())
}
pub(super) fn verify(
    job: &mut DuplicateJob<'_, '_>,
    guards: &[Option<&Guard>],
    cancel: &CancellationToken,
) -> Result<()> {
    if !ready(job)? {
        job.summary.complete &= job.summary.folder_unverified == 0;
        return Ok(());
    }
    if let Err(error) = verify_inner(job, guards, cancel) {
        job.summary.budget_limited |= match &error {
            Error::Io(e) => e.kind() == io::ErrorKind::QuotaExceeded,
            Error::Sql(rusqlite::Error::SqliteFailure(e, _)) => {
                e.code == rusqlite::ErrorCode::DiskFull
            }
            _ => false,
        };
        job.summary.folder_diagnostic = Some(error.to_string().chars().take(512).collect());
        // Failed folder work never removes verified standalone file results.
        job.db.execute_batch("DELETE FROM folder_groups;DELETE FROM folder_details;DELETE FROM folder_covered;UPDATE folder_nodes SET covered=NULL;")?;
        job.summary.folder_groups = 0;
        job.summary.folder_files = 0;
        job.summary.folder_logical_bytes = 0;
        job.summary.folder_unverified = job.summary.folder_unverified.saturating_add(1);
        job.summary.complete = false;
    }
    job.summary.complete &= job.summary.folder_unverified == 0;
    job.summary.source_independence_verified &= job.summary.complete;
    Ok(())
}

/// Metadata only. Read the complete inventory, including entries excluded by a
/// file query, so a selected duplicate prefix can never certify a whole tree.
fn prepare_inner(job: &mut DuplicateJob<'_, '_>, cancel: &CancellationToken) -> Result<()> {
    if !job.mode.is_cross_source() || job.summary.cancelled {
        return Ok(());
    }
    job.db.execute_batch("CREATE TABLE folder_nodes(id INTEGER PRIMARY KEY,scope INTEGER NOT NULL,side INTEGER NOT NULL,guard INTEGER NOT NULL,entry BLOB NOT NULL,parent_entry BLOB,parent INTEGER,name BLOB NOT NULL,path BLOB NOT NULL,kind INTEGER NOT NULL,size BLOB,modified_s INTEGER,modified_ns INTEGER,evidence BLOB,covered INTEGER,processed INTEGER NOT NULL DEFAULT 0,node_count BLOB,valid INTEGER NOT NULL DEFAULT 0,uncertain INTEGER NOT NULL DEFAULT 0,digest BLOB,files BLOB,logical BLOB,UNIQUE(scope,entry));
    CREATE INDEX folder_parent ON folder_nodes(parent,name);
    CREATE INDEX folder_digest ON folder_nodes(digest,scope,id); CREATE INDEX folder_candidates ON folder_nodes(side,kind,valid,processed,covered,node_count DESC,id);
    CREATE TABLE folder_groups(id INTEGER PRIMARY KEY,a INTEGER NOT NULL,b INTEGER NOT NULL,files BLOB NOT NULL,logical BLOB NOT NULL,c INTEGER);
    CREATE TABLE folder_details(folder BLOB NOT NULL,seq INTEGER NOT NULL,a INTEGER NOT NULL,b INTEGER NOT NULL,c INTEGER,PRIMARY KEY(folder,seq)) WITHOUT ROWID;
    CREATE TABLE folder_projection(seq INTEGER PRIMARY KEY,a INTEGER NOT NULL,b INTEGER NOT NULL,c INTEGER); CREATE TABLE folder_third(a INTEGER PRIMARY KEY,c INTEGER NOT NULL);
    CREATE TABLE folder_pending(seq INTEGER PRIMARY KEY,a INTEGER NOT NULL,b INTEGER NOT NULL,done INTEGER NOT NULL DEFAULT 0); CREATE TABLE folder_state(ready INTEGER NOT NULL); CREATE INDEX folder_pending_work ON folder_pending(done,seq);")?;
    let mut guard_base = 0usize;
    for (scope_index, scope) in job.scopes.iter().enumerate() {
        let query = scope.query;
        let evidence: bool = query._store.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='entry_evidence')", [], |r| r.get(0))?;
        let recorded_context = context(query._store.acquisition_context(query.generation)?);
        let mut located = paths::Paths::new(128 * 1024);
        let mut statement = query._store.db.prepare("SELECT id,kind,logical,modified_s,modified_ns,parent,name,present FROM entries WHERE generation=?1 AND present=1 ORDER BY id")?;
        let mut rows = statement.query([unsigned(query.generation.0)])?;
        while let Some(row) = rows.next()? {
            if cancel.is_cancelled() {
                job.summary.cancelled = true;
                return Ok(());
            }
            let id = EntryId(number(row, 0)?);
            // Paths uses the inventory's bounded native ancestor projection.
            let Some(p) = located.row(query._store, query.generation, id, &query.root, row)? else {
                continue;
            };
            let Some(guard) = scope.roots.iter().position(|r| p.starts_with(r)) else {
                continue;
            };
            let kind: i64 = row.get(1)?;
            let proof: Option<Vec<u8>> = if evidence {
                query
                    ._store
                    .db
                    .query_row(
                        "SELECT proof FROM entry_evidence WHERE generation=?1 AND id=?2",
                        params![unsigned(query.generation.0), unsigned(id.0)],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten()
            } else {
                None
            };
            let coverage = if kind == Kind::Directory as i64 {
                query._store.db.query_row("SELECT EXISTS(SELECT 1 FROM tasks t JOIN entries e ON e.generation=t.generation AND e.id=t.directory WHERE t.generation=?1 AND t.directory=?2 AND t.kind=0 AND t.listing=1 AND t.reconciled=1 AND t.parent_attempt=e.attempt AND t.key IS NOT NULL AND substr(t.key,1,length(?3))=?3)",params![unsigned(query.generation.0),unsigned(id.0),recorded_context],|r|r.get::<_,bool>(0))?
            } else {
                false
            };
            job.db.execute("INSERT INTO folder_nodes(scope,side,guard,entry,parent_entry,name,path,kind,size,modified_s,modified_ns,evidence,valid) VALUES(?1,?13,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![scope_index as i64,(guard_base+guard) as i64,unsigned(id.0),row.get::<_,Option<Vec<u8>>>(5)?,row.get::<_,Vec<u8>>(6)?,native_path(p).encode(),kind,row.get::<_,Option<Vec<u8>>>(2)?,row.get::<_,Option<i64>>(3)?,row.get::<_,Option<u32>>(4)?,proof,coverage,scope.side])?;
        }
        guard_base += scope.roots.len();
    }
    job.db.execute("UPDATE folder_nodes SET parent=(SELECT p.id FROM folder_nodes p WHERE p.scope=folder_nodes.scope AND p.entry=folder_nodes.parent_entry)", [])?;
    job.db.execute("INSERT INTO folder_state VALUES(1)", [])?;
    Ok(())
}

fn node_row(job: &DuplicateJob<'_, '_>, id: i64) -> Result<Row> {
    let (scope, entry, raw, kind, size): StoredNode = job.db.query_row(
        "SELECT scope,entry,path,kind,size FROM folder_nodes WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as usize,
                number(r, 1)?,
                r.get(2)?,
                r.get(3)?,
                opt_number(r, 4)?,
            ))
        },
    )?;
    let query = job.scopes[scope].query;
    Ok(Row {
        key: EntryKey {
            inventory: query._store.inventory,
            generation: query.generation,
            entry: EntryId(entry),
        },
        path: path(raw)?,
        kind: match kind {
            1 => Kind::File,
            2 => Kind::Directory,
            3 => Kind::Symlink,
            4 => Kind::Special,
            _ => Kind::Unknown,
        },
        logical: size,
    })
}
fn node_side(job: &DuplicateJob<'_, '_>, id: i64) -> Result<u8> {
    Ok(job
        .db
        .query_row("SELECT side FROM folder_nodes WHERE id=?1", [id], |r| {
            r.get(0)
        })?)
}
fn fresh(
    job: &DuplicateJob<'_, '_>,
    id: i64,
    guards: &[Option<&Guard>],
    verifier: &Verifier<'_>,
) -> Result<bool> {
    let (guard, raw, kind, modified_s, modified_ns, evidence): FreshRecord = job.db.query_row(
        "SELECT guard,path,kind,modified_s,modified_ns,evidence FROM folder_nodes WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as usize,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    let p = path(raw)?;
    if kind == Kind::Directory as i64 {
        let Some(expected) = time(modified_s, modified_ns) else {
            return Ok(false);
        };
        let Some(evidence) = evidence.filter(|e| !e.is_empty()) else {
            return Ok(false);
        };
        let Some(guard) = guards[guard] else {
            return Ok(false);
        };
        let Ok(stamp) = guard.inspect(&p) else {
            return Ok(false);
        };
        Ok(stamp.directory
            && stamp.modified == Some(expected)
            && crate::inventory::differential::NativeEvidence::from_stamp(&stamp).encode()
                == evidence)
    } else if kind == Kind::File as i64 {
        let proof:Option<Vec<u8>>=job.db.query_row("SELECT c.proof FROM candidates c JOIN folder_nodes n ON n.scope=c.scope AND n.entry=c.entry JOIN groups g ON g.id=c.group_id WHERE n.id=?1",[id],|r|r.get(0)).optional()?.flatten();
        let Some(proof) = proof else { return Ok(false) };
        let Some(guard) = guards[guard] else {
            return Ok(false);
        };
        Ok(verifier
            .validate(guard, &p, &Proof::decode(&proof)?)
            .is_ok())
    } else {
        Ok(false)
    }
}

/// Reuse the engine's native proof guards without reading a single content
/// byte. A stale member retracts its entire exact file group; other groups stay
/// available and the original error accounting remains authoritative.
fn revalidate_files(
    job: &mut DuplicateJob<'_, '_>,
    guards: &[Option<&Guard>],
    verifier: &Verifier<'_>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let mut after = 0u64;
    loop {
        if job.summary.errors >= job.max_errors as u64 || job.summary.budget_limited {
            job.summary.complete = false;
            return Ok(false);
        }
        if cancel.is_cancelled() {
            job.summary.cancelled = true;
            job.summary.complete = false;
            return Ok(false);
        }
        let row:Option<FileRevalidationRecord>=job.db.query_row("SELECT c.id,c.path,c.guard,c.proof,c.group_id,g.members,g.paths FROM candidates c JOIN groups g ON g.id=c.group_id WHERE c.id>?1 ORDER BY c.id LIMIT 1",[unsigned(after)],|r|Ok((number(r,0)?,r.get(1)?,r.get::<_,i64>(2)? as usize,r.get(3)?,number(r,4)?,number(r,5)?,number(r,6)?))).optional()?;
        let Some((id, raw, guard, bytes, group, members, paths)) = row else {
            break;
        };
        after = id;
        let validation = if let Some(guard) = guards[guard] {
            verifier.validate(guard, &path(raw)?, &Proof::decode(&bytes)?)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "duplicate source unavailable",
            ))
        };
        if let Err(error) = validation {
            job.db
                .execute("DELETE FROM groups WHERE id=?1", [unsigned(group)])?;
            job.summary.groups = job.summary.groups.saturating_sub(1);
            job.summary.independent_members =
                job.summary.independent_members.saturating_sub(members);
            job.summary.aliases = job
                .summary
                .aliases
                .saturating_sub(paths.saturating_sub(members));
            job.summary.complete = false;
            job.summary.source_independence_verified = false;
            if job.issue(id, error, cancel)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn verify_inner(
    job: &mut DuplicateJob<'_, '_>,
    guards: &[Option<&Guard>],
    cancel: &CancellationToken,
) -> Result<()> {
    if !job.mode.is_cross_source()
        || job.summary.cancelled
        || job.summary.budget_limited
        || cancel.is_cancelled()
    {
        return Ok(());
    }
    let verifier = Verifier::new(Default::default(), cancel);
    if !revalidate_files(job, guards, &verifier, cancel)? {
        job.db.execute_batch("DELETE FROM folder_groups;DELETE FROM folder_details;DELETE FROM folder_covered;UPDATE folder_nodes SET covered=NULL;")?;
        job.summary.folder_groups = 0;
        job.summary.folder_files = 0;
        job.summary.folder_logical_bytes = 0;
        return Ok(());
    }
    // Postorder uses acquisition IDs: every parent precedes its descendants.
    let mut after = i64::MAX;
    loop {
        if cancel.is_cancelled() {
            job.summary.cancelled = true;
            break;
        }
        let next: Option<(i64, usize, i64, bool)> = job
            .db
            .query_row(
                "SELECT id,scope,kind,valid FROM folder_nodes WHERE id<?1 ORDER BY id DESC LIMIT 1",
                [after],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, i64>(1)? as usize,
                        r.get(2)?,
                        r.get(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, scope, kind, coverage)) = next else {
            break;
        };
        after = id;
        let mut uncertain = job.root_issues.iter().any(|(s, _)| *s == scope)
            || job.root_warnings.iter().any(|(s, _)| *s == scope);
        let own_fresh = fresh(job, id, guards, &verifier)?;
        if kind == Kind::Directory as i64 {
            uncertain |= !coverage || !own_fresh;
        }
        let mut valid = !uncertain && (kind != Kind::Directory as i64 || coverage) && own_fresh;
        let mut digest = Sha256::new();
        digest.update([kind as u8]);
        let (mut files, mut logical, mut node_count) = (0u64, 0u64, 1u64);
        if kind == Kind::File as i64 {
            let selected:bool=job.db.query_row("SELECT EXISTS(SELECT 1 FROM candidates c JOIN folder_nodes n ON n.scope=c.scope AND n.entry=c.entry WHERE n.id=?1)",[id],|r|r.get(0))?;
            if !selected {
                job.summary.folder_filtered += 1;
            }
            let data:Option<(Vec<u8>,u64)>=job.db.query_row("SELECT c.group_id,c.size FROM candidates c JOIN folder_nodes n ON n.scope=c.scope AND n.entry=c.entry JOIN groups g ON g.id=c.group_id WHERE n.id=?1",[id],|r|Ok((r.get(0)?,number(r,1)?))).optional()?;
            if let Some((group, size)) = data {
                uncertain |= !own_fresh;
                digest.update(group);
                files = 1;
                logical = size;
            } else {
                valid = false;
            }
        } else if kind == Kind::Directory as i64 {
            let mut children=job.db.prepare("SELECT name,valid,digest,files,logical,uncertain,node_count FROM folder_nodes WHERE parent=?1 ORDER BY name")?;
            let mut rows = children.query([id])?;
            while let Some(row) = rows.next()? {
                if cancel.is_cancelled() {
                    job.summary.cancelled = true;
                    valid = false;
                    break;
                }
                let name: Vec<u8> = row.get(0)?;
                let child_valid: bool = row.get(1)?;
                let child_digest: Option<Vec<u8>> = row.get(2)?;
                valid &= child_valid;
                uncertain |= row.get::<_, bool>(5)?;
                digest.update((name.len() as u64).to_be_bytes());
                digest.update(name);
                if let Some(d) = child_digest {
                    digest.update(d)
                } else {
                    valid = false;
                }
                node_count = node_count
                    .checked_add(opt_number(row, 6)?.unwrap_or(1))
                    .ok_or(Error::Contract("folder node count overflow"))?;
                let f = opt_number(row, 3)?.unwrap_or(0);
                let b = opt_number(row, 4)?.unwrap_or(0);
                match (files.checked_add(f), logical.checked_add(b)) {
                    (Some(f), Some(b)) => {
                        files = f;
                        logical = b
                    }
                    _ => valid = false,
                }
            }
        } else {
            valid = false;
            uncertain = true;
        }
        if kind == Kind::Directory as i64 && uncertain {
            job.summary.folder_unverified += 1;
        }
        job.db.execute(
            "UPDATE folder_nodes SET valid=?2,digest=?3,files=?4,logical=?5,uncertain=?6,node_count=?7 WHERE id=?1",
            params![
                id,
                valid,
                digest.finalize().as_slice(),
                unsigned(files),
                unsigned(logical),
                uncertain,
                unsigned(node_count)
            ],
        )?;
    }
    if job.summary.cancelled {
        return Ok(());
    }
    if job.summary.folder_filtered > 0 && job.summary.folder_diagnostic.is_none() {
        job.summary.folder_diagnostic=Some("The file selection excludes inventoried entries; complete folder equality is not verified".into());
    }
    // Largest trees across all chosen sides precede every proper subtree.
    // A result consumes each native location only once. All optional third
    // mappings stay on disk and use proofs from this one verification job.
    // Fileless trees still contribute to their parent's digest and exact proof.
    loop {
        if cancel.is_cancelled() {
            job.summary.cancelled = true;
            break;
        }
        let seed: Option<(i64, u8, Vec<u8>, u64, u64)> = job.db.query_row(
            "SELECT n.id,n.side,n.digest,n.files,n.logical FROM folder_nodes n WHERE n.processed=0 AND n.kind=2 AND n.valid=1 AND n.files>?1 AND n.covered IS NULL ORDER BY n.node_count DESC,n.side,n.id LIMIT 1",
            [unsigned(0)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,number(r,3)?,number(r,4)?))
        ).optional()?;
        let Some((seed, seed_side, digest, files, logical)) = seed else {
            break;
        };
        job.db
            .execute("UPDATE folder_nodes SET processed=1 WHERE id=?1", [seed])?;
        let mut pair = None;
        let mut after = (0u8, 0i64);
        loop {
            let candidate: Option<(i64,u8)> = job.db.query_row(
                "SELECT id,side FROM folder_nodes WHERE (side,id)>(?1,?2) AND kind=2 AND valid=1 AND covered IS NULL AND digest=?3 AND side!=?4 ORDER BY side,id LIMIT 1",
                params![after.0,after.1,digest,seed_side], |r| Ok((r.get(0)?,r.get(1)?))
            ).optional()?;
            let Some((other, side)) = candidate else {
                break;
            };
            after = (side, other);
            let (a, b) = if seed_side < side {
                (seed, other)
            } else {
                (other, seed)
            };
            match exact(job, a, b, guards, &verifier, cancel)? {
                PairProof::Verified => {
                    job.db.execute_batch("DELETE FROM folder_projection; INSERT INTO folder_projection(seq,a,b) SELECT seq,a,b FROM folder_pending;")?;
                    pair = Some((a, b, seed_side.min(side), seed_side.max(side)));
                    break;
                }
                PairProof::Unverified => {
                    job.summary.folder_unverified = job.summary.folder_unverified.saturating_add(1);
                    job.summary.complete = false;
                }
                PairProof::Different => {}
            }
            if cancel.is_cancelled() {
                job.summary.cancelled = true;
                break;
            }
        }
        let Some((a, b, a_side, b_side)) = pair else {
            continue;
        };
        let mut third = None;
        if job.mode == Mode::BetweenThreeSources {
            let mut after = 0i64;
            loop {
                let candidate: Option<i64> = job.db.query_row(
                    "SELECT id FROM folder_nodes WHERE id>?1 AND kind=2 AND valid=1 AND covered IS NULL AND digest=?2 AND side NOT IN (?3,?4) ORDER BY id LIMIT 1",
                    params![after,digest,a_side,b_side], |r| r.get(0)
                ).optional()?;
                let Some(c) = candidate else {
                    break;
                };
                after = c;
                let ac = exact(job, a, c, guards, &verifier, cancel)?;
                if ac == PairProof::Verified {
                    job.db.execute_batch("DELETE FROM folder_third; INSERT INTO folder_third(a,c) SELECT a,b FROM folder_pending;")?;
                    // A/B and A/C alone do not establish B/C independence:
                    // B and C could be two aliases of the same native files.
                    let bc = exact(job, b, c, guards, &verifier, cancel)?;
                    if bc == PairProof::Verified {
                        job.db.execute("UPDATE folder_projection SET c=(SELECT t.c FROM folder_third t WHERE t.a=folder_projection.a)", [])?;
                        third = Some(c);
                        break;
                    }
                    if bc == PairProof::Unverified {
                        job.summary.folder_unverified =
                            job.summary.folder_unverified.saturating_add(1);
                        job.summary.complete = false;
                    }
                } else if ac == PairProof::Unverified {
                    job.summary.folder_unverified = job.summary.folder_unverified.saturating_add(1);
                    job.summary.complete = false;
                }
                if cancel.is_cancelled() {
                    job.summary.cancelled = true;
                    break;
                }
            }
        }
        if cancel.is_cancelled() {
            job.summary.cancelled = true;
            break;
        }
        let group = job.summary.folder_groups + 1;
        job.db.execute(
            "INSERT INTO folder_groups(id,a,b,files,logical,c) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                group as i64,
                a,
                b,
                unsigned(files),
                unsigned(logical),
                third
            ],
        )?;
        job.db.execute(
            "INSERT INTO folder_details SELECT ?1,seq,a,b,c FROM folder_projection WHERE seq>1",
            [unsigned(group)],
        )?;
        job.db.execute("INSERT OR IGNORE INTO folder_covered(candidate,folder) SELECT c.id,?1 FROM candidates c JOIN folder_nodes n ON n.scope=c.scope AND n.entry=c.entry WHERE n.id IN (SELECT a FROM folder_projection UNION ALL SELECT b FROM folder_projection UNION ALL SELECT c FROM folder_projection WHERE c IS NOT NULL)",[unsigned(group)])?;
        job.db.execute("UPDATE folder_nodes SET covered=?1 WHERE id IN (SELECT a FROM folder_projection UNION ALL SELECT b FROM folder_projection UNION ALL SELECT c FROM folder_projection WHERE c IS NOT NULL)",[group as i64])?;
        job.summary.folder_groups = group;
        job.summary.folder_files = job
            .summary
            .folder_files
            .checked_add(files)
            .ok_or(Error::Contract("folder file count overflow"))?;
        job.summary.folder_logical_bytes = job
            .summary
            .folder_logical_bytes
            .checked_add(logical)
            .ok_or(Error::Contract("folder logical size overflow"))?;
    }
    if !revalidate_files(job, guards, &verifier, cancel)? {
        job.db.execute_batch("DELETE FROM folder_groups;DELETE FROM folder_details;DELETE FROM folder_covered;UPDATE folder_nodes SET covered=NULL;")?;
        job.summary.folder_groups = 0;
        job.summary.folder_files = 0;
        job.summary.folder_logical_bytes = 0;
        return Ok(());
    }
    // A later pair must not leave an earlier pair carrying stale proofs.
    // Recheck current file handles without reading contents, then directory
    // stamps in postorder, so an added/removed child invalidates its group.
    if !job.summary.cancelled {
        let mut statement=job.db.prepare("SELECT id,covered FROM folder_nodes WHERE covered IS NOT NULL ORDER BY (kind=2),id DESC")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if cancel.is_cancelled() {
                job.summary.cancelled = true;
                break;
            }
            let id: i64 = row.get(0)?;
            let group: i64 = row.get(1)?;
            if !fresh(job, id, guards, &verifier)? {
                job.db
                    .execute("DELETE FROM folder_groups WHERE id=?1", [group])?;
                job.db.execute(
                    "DELETE FROM folder_details WHERE folder=?1",
                    [unsigned(group as u64)],
                )?;
                job.db.execute(
                    "DELETE FROM folder_covered WHERE folder=?1",
                    [unsigned(group as u64)],
                )?;
                job.db.execute(
                    "UPDATE folder_nodes SET covered=NULL WHERE covered=?1",
                    [group],
                )?;
                job.summary.folder_unverified = job.summary.folder_unverified.saturating_add(1);
            }
        }
        let mut groups = job
            .db
            .prepare("SELECT files,logical FROM folder_groups ORDER BY id")?;
        let mut rows = groups.query([])?;
        let (mut count, mut files, mut bytes) = (0u64, 0u64, 0u64);
        while let Some(row) = rows.next()? {
            count += 1;
            files = files
                .checked_add(number(row, 0)?)
                .ok_or(Error::Contract("folder file count overflow"))?;
            bytes = bytes
                .checked_add(number(row, 1)?)
                .ok_or(Error::Contract("folder logical size overflow"))?;
        }
        job.summary.folder_groups = count;
        job.summary.folder_files = files;
        job.summary.folder_logical_bytes = bytes;
    }
    if job.summary.cancelled {
        job.db.execute_batch("DELETE FROM folder_groups;DELETE FROM folder_details;DELETE FROM folder_covered;UPDATE folder_nodes SET covered=NULL;")?;
        job.summary.folder_groups = 0;
        job.summary.folder_files = 0;
        job.summary.folder_logical_bytes = 0;
        job.summary.complete = false;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum PairProof {
    Verified,
    Different,
    Unverified,
}

/// No hash decides equality. The work queue lives on disk and checks every
/// relative name/kind plus the existing exactly compared file group and proof.
fn exact(
    job: &DuplicateJob<'_, '_>,
    a: i64,
    b: i64,
    guards: &[Option<&Guard>],
    verifier: &Verifier<'_>,
    cancel: &CancellationToken,
) -> Result<PairProof> {
    job.db.execute("DELETE FROM folder_pending", [])?;
    job.db.execute(
        "INSERT INTO folder_pending(a,b) VALUES(?1,?2)",
        params![a, b],
    )?;
    loop {
        if cancel.is_cancelled() {
            return Ok(PairProof::Unverified);
        }
        let row: Option<(i64, i64, i64)> = job
            .db
            .query_row(
                "SELECT seq,a,b FROM folder_pending WHERE done=0 ORDER BY seq LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((seq, a, b)) = row else {
            let mut statement = job
                .db
                .prepare("SELECT a,b FROM folder_pending ORDER BY seq DESC")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                if cancel.is_cancelled()
                    || !fresh(job, row.get(0)?, guards, verifier)?
                    || !fresh(job, row.get(1)?, guards, verifier)?
                {
                    return Ok(PairProof::Unverified);
                }
            }
            return Ok(PairProof::Verified);
        };
        let overlap: bool = job.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM folder_nodes WHERE id IN (?1,?2) AND covered IS NOT NULL)",
            params![a, b],
            |r| r.get(0),
        )?;
        if overlap {
            return Ok(PairProof::Different);
        }
        if !fresh(job, a, guards, verifier)? || !fresh(job, b, guards, verifier)? {
            return Ok(PairProof::Unverified);
        }
        let (kind,ea,eb):(i64,Option<Vec<u8>>,Option<Vec<u8>>)=job.db.query_row("SELECT a.kind,a.evidence,b.evidence FROM folder_nodes a,folder_nodes b WHERE a.id=?1 AND b.id=?2",params![a,b],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if kind == Kind::Directory as i64 && ea == eb {
            return Ok(PairProof::Different);
        }
        if kind == Kind::File as i64 {
            let independent:bool=job.db.query_row("SELECT EXISTS(SELECT 1 FROM candidates ca JOIN folder_nodes a ON a.scope=ca.scope AND a.entry=ca.entry JOIN candidates cb JOIN folder_nodes b ON b.scope=cb.scope AND b.entry=cb.entry WHERE a.id=?1 AND b.id=?2 AND ca.group_id=cb.group_id AND ca.representative!=cb.representative)",params![a,b],|r|r.get(0))?;
            if !independent {
                return Ok(PairProof::Different);
            }
        } else {
            let mut left=job.db.prepare("SELECT n.id,n.name,n.kind,c.group_id,c.representative FROM folder_nodes n LEFT JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry WHERE n.parent=?1 ORDER BY n.name")?;
            let mut right=job.db.prepare("SELECT n.id,n.name,n.kind,c.group_id,c.representative FROM folder_nodes n LEFT JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry WHERE n.parent=?1 ORDER BY n.name")?;
            let mut l = left.query([a])?;
            let mut r = right.query([b])?;
            loop {
                if cancel.is_cancelled() {
                    return Ok(PairProof::Unverified);
                }
                let read =
                    |row: Option<&rusqlite::Row<'_>>| -> rusqlite::Result<Option<PairChild>> {
                        row.map(|x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?)))
                            .transpose()
                    };
                match (read(l.next()?)?, read(r.next()?)?) {
                    (None, None) => break,
                    (Some((li, ln, lk, lg, lr)), Some((ri, rn, rk, rg, rr)))
                        if ln == rn
                            && lk == rk
                            && (lk == 2 || (lg.is_some() && lg == rg && lr != rr)) =>
                    {
                        job.db.execute(
                            "INSERT INTO folder_pending(a,b) VALUES(?1,?2)",
                            params![li, ri],
                        )?;
                    }
                    _ => return Ok(PairProof::Different),
                }
            }
        }
        job.db
            .execute("UPDATE folder_pending SET done=1 WHERE seq=?1", [seq])?;
    }
}

impl DuplicateJob<'_, '_> {
    /// Read one recorded folder group without opening or inspecting a source.
    pub fn folder_group(&self, id: u64) -> Result<Option<FolderGroup>> {
        if !ready(self)? {
            return Ok(None);
        }
        let Ok(stored_id) = i64::try_from(id) else {
            return Ok(None);
        };
        let stored: Option<(i64, i64, Option<i64>, u64, u64)> = self
            .db
            .query_row(
                "SELECT a,b,c,files,logical FROM folder_groups WHERE id=?1",
                [stored_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        number(r, 3)?,
                        number(r, 4)?,
                    ))
                },
            )
            .optional()?;
        stored
            .map(|(a, b, c, files, logical_bytes)| {
                Ok(FolderGroup {
                    id,
                    a: node_row(self, a)?,
                    b: node_row(self, b)?,
                    c: c.map(|n| node_row(self, n)).transpose()?,
                    a_side: node_side(self, a)?,
                    b_side: node_side(self, b)?,
                    c_side: c.map(|n| node_side(self, n)).transpose()?,
                    files,
                    logical_bytes,
                    verified: true,
                })
            })
            .transpose()
    }
    pub fn folder_groups(&self, after: Option<u64>) -> Result<Vec<FolderGroup>> {
        self.folder_groups_direction(after, false)
    }
    pub fn folder_groups_before(&self, before: Option<u64>) -> Result<Vec<FolderGroup>> {
        self.folder_groups_direction(before, true)
    }
    fn folder_groups_direction(
        &self,
        cursor: Option<u64>,
        reverse: bool,
    ) -> Result<Vec<FolderGroup>> {
        if !ready(self)? {
            return Ok(Vec::new());
        }
        let sql = if reverse {
            "SELECT id,a,b,files,logical,c FROM folder_groups WHERE id<?1 ORDER BY id DESC LIMIT 100"
        } else {
            "SELECT id,a,b,files,logical,c FROM folder_groups WHERE id>?1 ORDER BY id LIMIT 100"
        };
        let mut statement = self.db.prepare(sql)?;
        let mut rows = statement.query([cursor
            .unwrap_or(if reverse { i64::MAX as u64 } else { 0 })
            .min(i64::MAX as u64) as i64])?;
        let mut out = Vec::new();
        let mut used = 0;
        while let Some(r) = rows.next()? {
            let a = node_row(self, r.get(1)?)?;
            let b = node_row(self, r.get(2)?)?;
            let c_id: Option<i64> = r.get(5)?;
            let c = c_id.map(|id| node_row(self, id)).transpose()?;
            used += 2
                * (a.path.as_os_str().as_encoded_bytes().len()
                    + b.path.as_os_str().as_encoded_bytes().len()
                    + c.as_ref()
                        .map_or(0, |r| r.path.as_os_str().as_encoded_bytes().len()))
                + 768;
            if used > 512 * 1024 {
                break;
            }
            out.push(FolderGroup {
                id: r.get::<_, i64>(0)? as u64,
                a,
                b,
                c,
                a_side: node_side(self, r.get(1)?)?,
                b_side: node_side(self, r.get(2)?)?,
                c_side: c_id.map(|id| node_side(self, id)).transpose()?,
                files: number(r, 3)?,
                logical_bytes: number(r, 4)?,
                verified: true,
            });
        }
        if reverse {
            out.reverse()
        };
        Ok(out)
    }
    pub fn folder_page(
        &self,
        group: u64,
        cursor: Option<&FolderCursor>,
        reverse: bool,
    ) -> Result<FolderPage> {
        if !ready(self)? {
            return Ok(FolderPage {
                rows: Vec::new(),
                first: None,
                last: None,
                more: false,
            });
        }
        if cursor.is_some_and(|c| c.owner != self.owner || c.group != group) {
            return Err(Error::Contract("foreign folder cursor"));
        }
        // A detail becomes one UI row per location. In triple mode the UI
        // also shows three selectable copy roots on the first page. Retain
        // the existing global ceiling of 200 rendered rows including them.
        let limit = if self.mode == Mode::BetweenThreeSources {
            65
        } else {
            100
        };
        let sql = if reverse {
            "SELECT seq,a,b,c FROM folder_details WHERE folder=?1 AND seq<?2 ORDER BY seq DESC LIMIT 101"
        } else {
            "SELECT seq,a,b,c FROM folder_details WHERE folder=?1 AND seq>?2 ORDER BY seq LIMIT 101"
        };
        let mut statement = self.db.prepare(sql)?;
        let mut rows = statement.query(params![
            unsigned(group),
            cursor
                .map(|c| c.seq)
                .unwrap_or(if reverse { i64::MAX } else { 0 })
        ])?;
        let mut out = Vec::new();
        let mut cursors = Vec::new();
        let mut used = 0;
        let mut more = false;
        while let Some(r) = rows.next()? {
            if out.len() == limit {
                more = true;
                break;
            }
            let a = node_row(self, r.get(1)?)?;
            let b = node_row(self, r.get(2)?)?;
            let c_id: Option<i64> = r.get(3)?;
            let c = c_id.map(|id| node_row(self, id)).transpose()?;
            used += 2
                * (a.path.as_os_str().as_encoded_bytes().len()
                    + b.path.as_os_str().as_encoded_bytes().len()
                    + c.as_ref()
                        .map_or(0, |r| r.path.as_os_str().as_encoded_bytes().len()))
                + 768;
            if used > 512 * 1024 {
                more = true;
                break;
            }
            cursors.push(FolderCursor {
                owner: self.owner.clone(),
                group,
                seq: r.get(0)?,
            });
            out.push(FolderDetail {
                a,
                b,
                c,
                a_side: node_side(self, r.get(1)?)?,
                b_side: node_side(self, r.get(2)?)?,
                c_side: c_id.map(|id| node_side(self, id)).transpose()?,
            });
        }
        if reverse {
            out.reverse();
            cursors.reverse()
        }
        Ok(FolderPage {
            first: cursors.first().cloned(),
            last: cursors.last().cloned(),
            rows: out,
            more,
        })
    }
}

#[cfg(test)]
mod tests;
