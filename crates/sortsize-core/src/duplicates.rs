//! Explicit content analysis; no deletion, link replacement or persistent cache.
use crate::{
    CancellationToken, InventoryKind, NativePath,
    analysis::{AnalysisScope, AnalysisSnapshot, SearchResult},
    content::{self, ContentId, Stamp},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{self, Read},
};
#[derive(Debug, Clone, Copy)]
pub struct DuplicateOptions {
    pub max_bytes_read: u64,
    pub max_file_bytes: u64,
    pub max_errors: usize,
}
impl Default for DuplicateOptions {
    fn default() -> Self {
        Self {
            max_bytes_read: 1024 * 1024 * 1024,
            max_file_bytes: 512 * 1024 * 1024,
            max_errors: 100,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct DuplicateMember {
    pub entry_indices: Vec<usize>,
    pub paths: Vec<NativePath>,
}
#[derive(Debug, Serialize)]
pub struct DuplicateGroup {
    pub logical_bytes_per_file: u64,
    pub sha256: String,
    pub members: Vec<DuplicateMember>,
}
#[derive(Debug, Serialize)]
pub struct DuplicateIssue {
    pub path: NativePath,
    pub reason: String,
    /// Typed presentation hint; preserve the analysis export contract.
    #[serde(skip)]
    pub error_kind: io::ErrorKind,
}
#[derive(Debug, Serialize)]
pub struct DuplicateReport {
    pub scope: AnalysisScope,
    pub selected_entries: usize,
    pub selected_entries_omitted: usize,
    pub groups: Vec<DuplicateGroup>,
    pub bytes_read: u64,
    pub files_hashed: u64,
    pub hardlink_aliases: u64,
    pub errors_count: u64,
    pub errors: Vec<DuplicateIssue>,
    pub cancelled: bool,
    pub budget_limited: bool,
    pub complete: bool,
    pub potentially_reclaimable_bytes: Option<u64>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct DuplicateProgress {
    pub bytes_read: u64,
    pub files_hashed: u64,
}
struct Candidate {
    index: usize,
    stamp: Stamp,
    aliases: Vec<usize>,
}
struct Reader<'a, F> {
    cancel: &'a CancellationToken,
    options: DuplicateOptions,
    progress: DuplicateProgress,
    notify: F,
}
impl<F: FnMut(DuplicateProgress)> Reader<'_, F> {
    fn read(&mut self, file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let remaining = self
            .options
            .max_bytes_read
            .saturating_sub(self.progress.bytes_read);
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::QuotaExceeded,
                "Content read budget exhausted",
            ));
        }
        let length = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = file.read(&mut buffer[..length])?;
        self.progress.bytes_read += read as u64;
        (self.notify)(self.progress);
        Ok(read)
    }
    fn hash(&mut self, file: &mut File, length: u64) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        let mut remaining = length;
        let mut buffer = [0u8; 65536];
        while remaining > 0 {
            let amount = buffer
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let n = self.read(file, &mut buffer[..amount])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "File changed"));
            }
            hash.update(&buffer[..n]);
            remaining -= n as u64;
        }
        Ok(hash.finalize().into())
    }
    fn equal(&mut self, a: &mut File, b: &mut File, length: u64) -> io::Result<bool> {
        // Empty candidates still need a cancellation point: no read() occurs
        // for them, and a large group must not revalidate every path on cancel.
        if self.cancel.is_cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let (mut aa, mut bb) = ([0u8; 65536], [0u8; 65536]);
        let mut remaining = length;
        while remaining > 0 {
            let amount = aa
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            // Network reads may be short: fill each side independently.
            for (file, buffer) in [(&mut *a, &mut aa[..amount]), (&mut *b, &mut bb[..amount])] {
                let mut filled = 0;
                while filled < amount {
                    let n = self.read(file, &mut buffer[filled..])?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "File changed"));
                    }
                    filled += n;
                }
            }
            if aa[..amount] != bb[..amount] {
                return Ok(false);
            }
            remaining -= amount as u64;
        }
        Ok(true)
    }
}
fn open(
    snapshot: &AnalysisSnapshot,
    index: usize,
    expected: Option<Stamp>,
) -> io::Result<(File, Stamp)> {
    let entry = &snapshot.entries[index];
    let path = entry.path.as_path();
    let root = snapshot
        .entries
        .first()
        .ok_or(io::ErrorKind::InvalidInput)?
        .path
        .as_path();
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    crate::validate_directory_scope(root, parent).map_err(io::Error::other)?;
    let file = content::open(path)?;
    let stamp = content::stamp(&file)?;
    if entry.logical_bytes != Some(stamp.length)
        || entry.modified.is_some_and(|m| m != stamp.modified)
        || expected.is_some_and(|e| e != stamp)
    {
        return Err(io::Error::other("File changed since observation"));
    }
    Ok((file, stamp))
}
fn stable(snapshot: &AnalysisSnapshot, index: usize, file: &File, stamp: Stamp) -> io::Result<()> {
    if content::stamp(file)? != stamp {
        return Err(io::Error::other("File changed while reading"));
    }
    let (_, current) = open(snapshot, index, Some(stamp))?;
    if current != stamp {
        return Err(io::Error::other("File replaced while reading"));
    }
    Ok(())
}
/// Every SHA match is byte-compared under fresh, verified handles. Hardlink
/// aliases are one member. Linux opens every component relative to a descriptor;
/// Windows rejects reparses and validates ancestry but has a documented path race.
/// No result is a promise that files remain equal after these observations.
pub fn find(
    snapshot: &AnalysisSnapshot,
    selection: &SearchResult,
    options: DuplicateOptions,
    cancel: &CancellationToken,
    notify: impl FnMut(DuplicateProgress),
) -> Result<DuplicateReport, String> {
    if snapshot.entries.len() > crate::analysis::MAX_ANALYSIS_ENTRIES
        || options.max_errors > 1000
        || options.max_bytes_read == 0
        || selection
            .indices
            .iter()
            .any(|&i| i >= snapshot.entries.len())
    {
        return Err("Invalid duplicate limits or selection".into());
    }
    let mut seen = HashSet::new();
    if selection.indices.iter().any(|&i| !seen.insert(i)) {
        return Err("Duplicate selection IDs".into());
    }
    let mut report = DuplicateReport {
        scope: snapshot.scope(),
        selected_entries: selection.indices.len(),
        selected_entries_omitted: selection.omitted_results,
        groups: vec![],
        bytes_read: 0,
        files_hashed: 0,
        hardlink_aliases: 0,
        errors_count: 0,
        errors: vec![],
        cancelled: false,
        budget_limited: false,
        complete: false,
        potentially_reclaimable_bytes: None,
    };
    let mut reader = Reader {
        cancel,
        options,
        progress: DuplicateProgress::default(),
        notify,
    };
    let mut sizes = BTreeMap::<u64, Vec<usize>>::new();
    for &i in &selection.indices {
        let e = &snapshot.entries[i];
        if e.kind == InventoryKind::File
            && let Some(size) = e.logical_bytes
        {
            sizes.entry(size).or_default().push(i);
        }
    }
    let issue = |index: usize, error: io::Error, report: &mut DuplicateReport| {
        if error.kind() == io::ErrorKind::Interrupted && cancel.is_cancelled() {
            report.cancelled = true;
            return;
        }
        if error.kind() == io::ErrorKind::QuotaExceeded {
            report.budget_limited = true;
            return;
        }
        report.errors_count += 1;
        if report.errors.len() < options.max_errors {
            report.errors.push(DuplicateIssue {
                path: NativePath::from(snapshot.entries[index].path.as_path()),
                reason: error.to_string(),
                error_kind: error.kind(),
            });
        }
    };
    'sizes: for (size, mut indices) in sizes {
        if indices.len() < 2 {
            continue;
        }
        indices.sort_by(|&a, &b| snapshot.entries[a].path.cmp(&snapshot.entries[b].path));
        if size > options.max_file_bytes {
            report.budget_limited = true;
            continue;
        }
        let mut hashes = BTreeMap::<[u8; 32], Vec<Candidate>>::new();
        let mut identities = HashMap::<ContentId, ([u8; 32], usize)>::new();
        for index in indices {
            if cancel.is_cancelled() {
                report.cancelled = true;
                break 'sizes;
            }
            let attempt = (|| -> io::Result<()> {
                let (mut file, stamp) = open(snapshot, index, None)?;
                if let Some(&(hash, position)) = identities.get(&stamp.identity) {
                    let candidate = &mut hashes.get_mut(&hash).expect("stored hash")[position];
                    if candidate.stamp != stamp {
                        return Err(io::Error::other("Hardlink changed"));
                    }
                    candidate.aliases.push(index);
                    report.hardlink_aliases += 1;
                    return Ok(());
                }
                let hash = reader.hash(&mut file, size)?;
                stable(snapshot, index, &file, stamp)?;
                reader.progress.files_hashed += 1;
                (reader.notify)(reader.progress);
                let group = hashes.entry(hash).or_default();
                identities.insert(stamp.identity, (hash, group.len()));
                group.push(Candidate {
                    index,
                    stamp,
                    aliases: vec![index],
                });
                Ok(())
            })();
            if let Err(error) = attempt {
                issue(index, error, &mut report);
                if report.cancelled || report.budget_limited {
                    break 'sizes;
                }
            }
        }
        for (hash, candidates) in hashes {
            if candidates.len() < 2 {
                continue;
            }
            let first = &candidates[0];
            let mut members = vec![];
            let mut valid = true;
            for candidate in candidates.iter().skip(1) {
                let attempt = (|| -> io::Result<bool> {
                    let (mut a, sa) = open(snapshot, first.index, Some(first.stamp))?;
                    let (mut b, sb) = open(snapshot, candidate.index, Some(candidate.stamp))?;
                    let equal = reader.equal(&mut a, &mut b, size)?;
                    stable(snapshot, first.index, &a, sa)?;
                    stable(snapshot, candidate.index, &b, sb)?;
                    Ok(equal)
                })();
                match attempt {
                    Ok(true) => {}
                    Ok(false) => {
                        issue(
                            candidate.index,
                            io::Error::other("Hash collision or concurrent mutation"),
                            &mut report,
                        );
                        valid = false;
                        break;
                    }
                    Err(error) => {
                        issue(candidate.index, error, &mut report);
                        valid = false;
                        break;
                    }
                }
            }
            if valid {
                // Revalidate every alias too; an alias may have been replaced since hashing.
                for candidate in &candidates {
                    for &index in &candidate.aliases {
                        if cancel.is_cancelled() {
                            report.cancelled = true;
                            valid = false;
                            break;
                        }
                        if let Err(error) = open(snapshot, index, Some(candidate.stamp)) {
                            issue(index, error, &mut report);
                            valid = false;
                            break;
                        }
                    }
                    if !valid {
                        break;
                    }
                }
                if valid {
                    for candidate in candidates {
                        members.push(DuplicateMember {
                            entry_indices: candidate.aliases.clone(),
                            paths: candidate
                                .aliases
                                .iter()
                                .map(|&i| NativePath::from(snapshot.entries[i].path.as_path()))
                                .collect(),
                        });
                    }
                    report.groups.push(DuplicateGroup {
                        logical_bytes_per_file: size,
                        sha256: hash.iter().map(|b| format!("{b:02x}")).collect(),
                        members,
                    });
                }
            }
            if report.cancelled || report.budget_limited {
                break 'sizes;
            }
        }
    }
    report.bytes_read = reader.progress.bytes_read;
    report.files_hashed = reader.progress.files_hashed;
    report.cancelled |= cancel.is_cancelled();
    report.complete = report.scope.exhaustive
        && selection.omitted_results == 0
        && selection.complete
        && selection.statistics.totals.unknown_sizes == 0
        && selection.unknown_dates == 0
        && selection.unsearchable_native_text == 0
        && !selection.cancelled
        && !report.cancelled
        && !report.budget_limited
        && report.errors_count == 0;
    Ok(report)
}
