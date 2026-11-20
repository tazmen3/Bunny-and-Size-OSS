//! Reversible review selection in the quota-owned comparison database.
//! These operations consult recorded results only: they never inspect a source,
//! reopen content, hash, scan, reconnect, or claim continuing freshness.
use super::*;

/// Stable identity within one leased comparison, never a visible row index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewTarget {
    Candidate(u64),
    Directory(i64),
    Folder(u64),
    FolderLocation { group: u64, side: u8 },
}

/// Presentation translates these codes; no provider error or path is embedded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReason {
    Eligible,
    ChooseReference,
    ProtectedReference,
    MissingReference,
    IncompleteEvidence,
    Alias,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewState {
    pub eligible: bool,
    pub selected: bool,
    pub partial: bool,
    pub reason: ReviewReason,
}
impl ReviewState {
    fn excluded(reason: ReviewReason) -> Self {
        Self {
            eligible: false,
            selected: false,
            partial: false,
            reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSummary {
    /// Selected file locations, including descendants and offscreen pages.
    /// Folder summaries are never added to these files or their logical bytes.
    pub files: u64,
    /// Fully selected highest folder locations outside the reference, informational only.
    pub folders: u64,
    pub logical_bytes: u64,
    pub eligible_files: u64,
    pub eligible_folders: u64,
    pub eligible_logical_bytes: u64,
    /// Empty leaf directories have no file/byte contribution.
    pub empty_directories: u64,
    pub eligible_empty_directories: u64,
    pub reference: Option<u8>,
    pub reason: ReviewReason,
}

// An alias across any chosen sides is never an independently selectable copy.
// This matches the per-row policy even when the alias is outside the reference.
// The reference must also have an independently verified, different identity.
const ELIGIBLE: &str = "c.side!=?1 AND c.representative=c.id AND c.proof IS NOT NULL AND c.hash IS NOT NULL AND NOT EXISTS(SELECT 1 FROM candidates alias WHERE alias.side!=c.side AND alias.identity=c.identity) AND NOT EXISTS(SELECT 1 FROM issues i WHERE i.id=c.id) AND EXISTS(SELECT 1 FROM candidates p WHERE p.group_id=c.group_id AND p.side=?1 AND p.representative=p.id AND p.proof IS NOT NULL AND p.hash IS NOT NULL AND p.identity!=c.identity AND NOT EXISTS(SELECT 1 FROM issues i WHERE i.id=p.id)) AND NOT EXISTS(SELECT 1 FROM folder_nodes covered JOIN folder_groups fg ON fg.id=covered.covered WHERE covered.scope=c.scope AND covered.entry=c.entry AND NOT EXISTS(SELECT 1 FROM folder_nodes reference_root WHERE reference_root.id IN(fg.a,fg.b,fg.c) AND reference_root.side=?1 AND reference_root.valid=1))";
const DESCENDANTS: &str = "WITH RECURSIVE subtree(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM folder_nodes n JOIN subtree s ON n.parent=s.id) ";

impl DuplicateJob<'_, '_> {
    pub(super) fn review_reference(&self) -> Result<Option<u8>> {
        Ok(self
            .db
            .query_row("SELECT reference FROM review_control", [], |r| r.get(0))?)
    }
    fn review_reason(&self) -> Result<ReviewReason> {
        Ok(if self.review_reference()?.is_none() {
            ReviewReason::ChooseReference
        } else if !self.mode.is_cross_source()
            || !self.summary.verified
            || !self.summary.complete
            || !self.summary.source_independence_verified
            || self.summary.unverified_sources != 0
            || self.summary.ambiguous_aliases != 0
            || self.summary.cancelled
            || self.summary.budget_limited
        {
            ReviewReason::IncompleteEvidence
        } else {
            ReviewReason::Eligible
        })
    }
    fn review_folders_ready(&self) -> Result<bool> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='folder_state')",
            [],
            |r| r.get(0),
        )?;
        Ok(exists
            && self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM folder_state WHERE ready=1)",
                [],
                |r| r.get(0),
            )?)
    }
    /// Choosing or changing a reference clears every mark. No default reference
    /// is inferred, and choosing one never selects content.
    pub fn set_protected_reference(&mut self, reference: Option<u8>) -> Result<()> {
        if reference.is_some_and(|side| side >= self.mode.side_count())
            || (reference.is_some() && !self.mode.is_cross_source())
        {
            return Err(Error::Contract("invalid protected reference"));
        }
        if self.current.is_some() {
            return Ok(());
        }
        self.clear_review_selection()?;
        self.db
            .execute("UPDATE review_control SET reference=?1", [reference])?;
        Ok(())
    }
    pub fn clear_review_selection(&mut self) -> Result<()> {
        self.db
            .execute_batch("DELETE FROM review_selected; DELETE FROM review_empty_directories;")?;
        Ok(())
    }
    /// Resolve the full inventory/generation/entry key and native path in the
    /// leased job. Keys from another comparison cannot resolve by row number.
    pub fn review_row_target(&self, row: &Row) -> Result<Option<ReviewTarget>> {
        if row.kind == Kind::File {
            return Ok(self.db.query_row("SELECT id FROM candidates WHERE inventory=?1 AND generation=?2 AND entry=?3 AND path=?4", params![row.key.inventory.0.to_be_bytes().as_slice(),unsigned(row.key.generation.0),unsigned(row.key.entry.0),native_path(&row.path).encode()], |r| number(r,0)).optional()?.map(ReviewTarget::Candidate));
        }
        if row.kind != Kind::Directory || !self.review_folders_ready()? {
            return Ok(None);
        }
        for (index, scope) in self.scopes.iter().enumerate() {
            if scope.query._store.inventory == row.key.inventory
                && scope.query.generation == row.key.generation
            {
                let node: Option<i64> = self.db.query_row("SELECT id FROM folder_nodes WHERE scope=?1 AND entry=?2 AND path=?3 AND kind=2", params![index as i64,unsigned(row.key.entry.0),native_path(&row.path).encode()], |r| r.get(0)).optional()?;
                if let Some(node) = node {
                    return Ok(Some(ReviewTarget::Directory(node)));
                }
            }
        }
        Ok(None)
    }
    pub fn review_candidate_for_row(&self, row: &Row) -> Result<Option<u64>> {
        Ok(match self.review_row_target(row)? {
            Some(ReviewTarget::Candidate(id)) => Some(id),
            _ => None,
        })
    }
    fn review_folder_nodes(&self, group: u64) -> Result<Vec<(u8, i64)>> {
        if !self.review_folders_ready()? {
            return Ok(Vec::new());
        }
        Ok(self.db.prepare("SELECT n.side,n.id FROM folder_groups g JOIN folder_nodes n ON n.id IN(g.a,g.b,g.c) WHERE g.id=?1 ORDER BY n.side,n.id LIMIT 3")?
            .query_map([group as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn review_directory(&self, target: ReviewTarget) -> Result<Option<i64>> {
        if !self.review_folders_ready()? {
            return Ok(None);
        }
        match target {
            ReviewTarget::Directory(node) => Ok(Some(node)),
            ReviewTarget::FolderLocation { group, side } => Ok(self
                .review_folder_nodes(group)?
                .into_iter()
                .find(|(chosen, _)| *chosen == side)
                .map(|(_, node)| node)),
            ReviewTarget::Folder(group) => Ok(self
                .review_folder_nodes(group)?
                .into_iter()
                .find(|(side, _)| Some(*side) != self.review_reference().ok().flatten())
                .map(|(_, node)| node)),
            ReviewTarget::Candidate(_) => Ok(None),
        }
    }
    pub fn review_state(&self, target: ReviewTarget) -> Result<ReviewState> {
        let reference = self.review_reference()?;
        let global = self.review_reason()?;
        if let ReviewTarget::Folder(group) = target {
            if global != ReviewReason::Eligible {
                return Ok(ReviewState::excluded(global));
            }
            let nodes = self.review_folder_nodes(group)?;
            if !nodes.iter().any(|(side, _)| Some(*side) == reference) {
                return Ok(ReviewState::excluded(ReviewReason::MissingReference));
            }
            let mut all = true;
            let mut any = false;
            let mut count = 0;
            for (side, node) in nodes {
                if Some(side) == reference {
                    continue;
                }
                let state = self.review_state(ReviewTarget::Directory(node))?;
                if !state.eligible {
                    return Ok(state);
                }
                all &= state.selected;
                any |= state.selected || state.partial;
                count += 1;
            }
            return Ok(ReviewState {
                eligible: count > 0,
                selected: count > 0 && all,
                partial: any && !all,
                reason: if count > 0 {
                    ReviewReason::Eligible
                } else {
                    ReviewReason::Unverified
                },
            });
        }
        if let ReviewTarget::Candidate(id) = target {
            let candidate: Option<(u8, Option<bool>, bool)> = self
                .db
                .query_row(
                    "SELECT c.side,c.representative=c.id,EXISTS(SELECT 1 FROM candidates p WHERE p.side!=c.side AND p.identity=c.identity) FROM candidates c WHERE c.id=?1",
                    [unsigned(id)],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((side, independent, cross_alias)) = candidate else {
                return Ok(ReviewState::excluded(ReviewReason::Unverified));
            };
            if Some(side) == reference {
                return Ok(ReviewState::excluded(ReviewReason::ProtectedReference));
            }
            if global != ReviewReason::Eligible {
                return Ok(ReviewState::excluded(global));
            }
            if independent == Some(false) || cross_alias {
                return Ok(ReviewState::excluded(ReviewReason::Alias));
            }
            if independent.is_none() {
                return Ok(ReviewState::excluded(ReviewReason::Unverified));
            }
            let eligible: bool = self.db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM candidates c JOIN groups g ON g.id=c.group_id WHERE c.id=?2 AND {ELIGIBLE})"),params![reference,unsigned(id)],|r|r.get(0))?;
            if !eligible {
                let missing: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM candidates c JOIN groups g ON g.id=c.group_id WHERE c.id=?2 AND NOT EXISTS(SELECT 1 FROM candidates p WHERE p.group_id=c.group_id AND p.side=?1 AND p.representative=p.id AND p.proof IS NOT NULL AND p.hash IS NOT NULL AND NOT EXISTS(SELECT 1 FROM issues i WHERE i.id=p.id)))", params![reference,unsigned(id)], |r| r.get(0))?;
                let missing_folder: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM candidates c JOIN folder_nodes covered ON covered.scope=c.scope AND covered.entry=c.entry JOIN folder_groups fg ON fg.id=covered.covered WHERE c.id=?2 AND NOT EXISTS(SELECT 1 FROM folder_nodes p WHERE p.id IN(fg.a,fg.b,fg.c) AND p.side=?1 AND p.valid=1))", params![reference,unsigned(id)], |r| r.get(0))?;
                return Ok(ReviewState::excluded(if missing || missing_folder {
                    ReviewReason::MissingReference
                } else {
                    ReviewReason::Unverified
                }));
            }
            let selected = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM review_selected WHERE candidate=?1)",
                [unsigned(id)],
                |r| r.get(0),
            )?;
            return Ok(ReviewState {
                eligible: true,
                selected,
                partial: false,
                reason: ReviewReason::Eligible,
            });
        }
        let Some(node) = self.review_directory(target)? else {
            return Ok(ReviewState::excluded(if global == ReviewReason::Eligible {
                ReviewReason::Unverified
            } else {
                global
            }));
        };
        let data: Option<(u8,bool)> = self.db.query_row("SELECT side,valid=1 AND covered IS NOT NULL AND EXISTS(SELECT 1 FROM folder_groups g WHERE g.id=folder_nodes.covered) FROM folder_nodes WHERE id=?1 AND kind=2",[node],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((side, valid)) = data else {
            return Ok(ReviewState::excluded(ReviewReason::Unverified));
        };
        if Some(side) == reference {
            return Ok(ReviewState::excluded(ReviewReason::ProtectedReference));
        }
        if global != ReviewReason::Eligible {
            return Ok(ReviewState::excluded(global));
        }
        if !valid {
            return Ok(ReviewState::excluded(ReviewReason::Unverified));
        }
        let has_reference: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM folder_nodes n JOIN folder_groups g ON g.id=n.covered JOIN folder_nodes p ON p.id IN(g.a,g.b,g.c) WHERE n.id=?1 AND p.side=?2 AND p.valid=1)",params![node,reference],|r|r.get(0))?;
        if !has_reference {
            return Ok(ReviewState::excluded(ReviewReason::MissingReference));
        }
        // Empty leaves must not bypass the exclusion of their covered copy.
        // The scope is this group location, not other independent locations.
        let location_alias: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM folder_nodes root JOIN folder_nodes n ON n.covered=root.covered AND n.side=root.side JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry WHERE root.id=?1 AND EXISTS(SELECT 1 FROM candidates alias WHERE alias.side!=c.side AND alias.identity=c.identity))",[node],|r|r.get(0))?;
        if location_alias {
            return Ok(ReviewState::excluded(ReviewReason::Alias));
        }
        let (files,selected): (u64,u64) = self.db.query_row(&format!("{DESCENDANTS} SELECT count(*),count(s.candidate) FROM subtree t JOIN folder_nodes n ON n.id=t.id JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry JOIN groups g ON g.id=c.group_id LEFT JOIN review_selected s ON s.candidate=c.id WHERE {}",ELIGIBLE.replace("?1","?2")),params![node,reference],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64)))?;
        let total_files:u64=self.db.query_row(&format!("{DESCENDANTS} SELECT count(*) FROM subtree t JOIN folder_nodes n ON n.id=t.id WHERE n.kind=1"),[node],|r|r.get::<_,i64>(0).map(|n|n as u64))?;
        if total_files != files {
            let alias:bool=self.db.query_row(&format!("{DESCENDANTS} SELECT EXISTS(SELECT 1 FROM subtree t JOIN folder_nodes n ON n.id=t.id JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry WHERE c.representative!=c.id OR EXISTS(SELECT 1 FROM candidates p WHERE p.side!=c.side AND p.identity=c.identity))"),[node],|r|r.get(0))?;
            return Ok(ReviewState::excluded(if alias {
                ReviewReason::Alias
            } else {
                ReviewReason::Unverified
            }));
        }
        let (empty,selected_empty): (u64,u64) = self.db.query_row(&format!("{DESCENDANTS} SELECT count(*),count(s.node) FROM subtree t JOIN folder_nodes n ON n.id=t.id LEFT JOIN review_empty_directories s ON s.node=n.id WHERE n.kind=2 AND NOT EXISTS(SELECT 1 FROM folder_nodes child WHERE child.parent=n.id)"),[node],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64)))?;
        let total = files + empty;
        let marked = selected + selected_empty;
        Ok(ReviewState {
            eligible: true,
            selected: total > 0 && total == marked,
            partial: marked > 0 && marked < total,
            reason: ReviewReason::Eligible,
        })
    }
    pub fn review_candidate_state(&self, id: u64) -> Result<ReviewState> {
        self.review_state(ReviewTarget::Candidate(id))
    }
    pub fn review_folder_state(&self, id: u64) -> Result<ReviewState> {
        self.review_state(ReviewTarget::Folder(id))
    }
    pub fn set_review_candidate(&mut self, id: u64, selected: bool) -> Result<()> {
        self.set_review_target(ReviewTarget::Candidate(id), selected)
    }
    pub fn set_review_folder(&mut self, id: u64, selected: bool) -> Result<()> {
        self.set_review_target(ReviewTarget::Folder(id), selected)
    }
    pub fn set_review_target(&mut self, target: ReviewTarget, selected: bool) -> Result<()> {
        if !self.review_state(target)?.eligible {
            return Err(Error::Contract("review target is not eligible"));
        }
        if let ReviewTarget::Folder(group) = target {
            let reference = self.review_reference()?;
            for (side, node) in self.review_folder_nodes(group)? {
                if Some(side) != reference {
                    self.set_review_target(ReviewTarget::Directory(node), selected)?;
                }
            }
            return Ok(());
        }
        if let ReviewTarget::Candidate(id) = target {
            if selected {
                self.db.execute(
                    "INSERT OR IGNORE INTO review_selected VALUES(?1)",
                    [unsigned(id)],
                )?;
            } else {
                self.db.execute(
                    "DELETE FROM review_selected WHERE candidate=?1",
                    [unsigned(id)],
                )?;
            }
            return Ok(());
        }
        let node = self
            .review_directory(target)?
            .ok_or(Error::Contract("unknown review directory"))?;
        let reference = self.review_reference()?;
        if selected {
            self.db.execute(&format!("{DESCENDANTS} INSERT OR IGNORE INTO review_selected SELECT c.id FROM subtree t JOIN folder_nodes n ON n.id=t.id JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry JOIN groups g ON g.id=c.group_id WHERE {}",ELIGIBLE.replace("?1","?2")),params![node,reference])?;
            self.db.execute(&format!("{DESCENDANTS} INSERT OR IGNORE INTO review_empty_directories SELECT n.id FROM subtree t JOIN folder_nodes n ON n.id=t.id WHERE n.kind=2 AND NOT EXISTS(SELECT 1 FROM folder_nodes child WHERE child.parent=n.id)"),[node])?;
        } else {
            self.db.execute(&format!("{DESCENDANTS} DELETE FROM review_selected WHERE candidate IN (SELECT c.id FROM subtree t JOIN folder_nodes n ON n.id=t.id JOIN candidates c ON c.scope=n.scope AND c.entry=n.entry)"),[node])?;
            self.db.execute(&format!("{DESCENDANTS} DELETE FROM review_empty_directories WHERE node IN (SELECT id FROM subtree)"),[node])?;
        }
        Ok(())
    }
    /// Global scope includes eligible copies on pages never loaded by the UI.
    pub fn select_all_review_copies(&mut self) -> Result<()> {
        if self.review_summary()?.reason != ReviewReason::Eligible {
            return Err(Error::Contract(
                "review comparison evidence is insufficient",
            ));
        }
        self.db.execute(&format!("INSERT OR IGNORE INTO review_selected SELECT c.id FROM candidates c JOIN groups g ON g.id=c.group_id WHERE {ELIGIBLE}"),[self.review_reference()?])?;
        if self.review_folders_ready()? {
            self.db.execute("INSERT OR IGNORE INTO review_empty_directories SELECT n.id FROM folder_nodes n JOIN folder_groups g ON g.id=n.covered WHERE n.side!=?1 AND n.kind=2 AND n.valid=1 AND EXISTS(SELECT 1 FROM folder_nodes p WHERE p.id IN(g.a,g.b,g.c) AND p.side=?1 AND p.valid=1) AND NOT EXISTS(SELECT 1 FROM folder_nodes location JOIN candidates c ON c.scope=location.scope AND c.entry=location.entry WHERE location.covered=n.covered AND location.side=n.side AND EXISTS(SELECT 1 FROM candidates alias WHERE alias.side!=c.side AND alias.identity=c.identity)) AND NOT EXISTS(SELECT 1 FROM folder_nodes child WHERE child.parent=n.id)",[self.review_reference()?])?;
        }
        Ok(())
    }
    /// Constant host memory, unsigned checked sums, and no SQL integer overflow.
    pub fn review_summary(&self) -> Result<ReviewSummary> {
        let reference = self.review_reference()?;
        let reason = self.review_reason()?;
        let mut out = ReviewSummary {
            files: 0,
            folders: 0,
            logical_bytes: 0,
            eligible_files: 0,
            eligible_folders: 0,
            eligible_logical_bytes: 0,
            empty_directories: 0,
            eligible_empty_directories: 0,
            reference,
            reason,
        };
        if reason != ReviewReason::Eligible {
            return Ok(out);
        }
        let mut statement=self.db.prepare(&format!("SELECT c.size,s.candidate IS NOT NULL FROM candidates c JOIN groups g ON g.id=c.group_id LEFT JOIN review_selected s ON s.candidate=c.id WHERE {ELIGIBLE}"))?;
        let mut rows = statement.query([reference])?;
        while let Some(row) = rows.next()? {
            let size = number(row, 0)?;
            out.eligible_files += 1;
            out.eligible_logical_bytes = out
                .eligible_logical_bytes
                .checked_add(size)
                .ok_or(Error::Contract("review logical size overflow"))?;
            if row.get::<_, bool>(1)? {
                out.files += 1;
                out.logical_bytes = out
                    .logical_bytes
                    .checked_add(size)
                    .ok_or(Error::Contract("review logical size overflow"))?;
            }
        }
        if self.review_folders_ready()? {
            let mut statement = self
                .db
                .prepare("SELECT id FROM folder_groups ORDER BY id")?;
            let mut groups = statement.query([])?;
            while let Some(group) = groups.next()? {
                for (side, node) in self.review_folder_nodes(group.get::<_, i64>(0)? as u64)? {
                    if Some(side) == reference {
                        continue;
                    }
                    let state = self.review_state(ReviewTarget::Directory(node))?;
                    if state.eligible {
                        out.eligible_folders += 1;
                    }
                    if state.selected {
                        out.folders += 1;
                    }
                }
            }
            (out.eligible_empty_directories,out.empty_directories)=self.db.query_row("SELECT count(*),count(s.node) FROM folder_nodes n JOIN folder_groups g ON g.id=n.covered LEFT JOIN review_empty_directories s ON s.node=n.id WHERE n.side!=?1 AND n.kind=2 AND n.valid=1 AND EXISTS(SELECT 1 FROM folder_nodes p WHERE p.id IN(g.a,g.b,g.c) AND p.side=?1 AND p.valid=1) AND NOT EXISTS(SELECT 1 FROM folder_nodes location JOIN candidates c ON c.scope=location.scope AND c.entry=location.entry WHERE location.covered=n.covered AND location.side=n.side AND EXISTS(SELECT 1 FROM candidates alias WHERE alias.side!=c.side AND alias.identity=c.identity)) AND NOT EXISTS(SELECT 1 FROM folder_nodes child WHERE child.parent=n.id)",[reference],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64)))?;
        }
        if out.eligible_files == 0 && out.eligible_empty_directories == 0 {
            let missing: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM groups g WHERE NOT EXISTS(SELECT 1 FROM candidates p WHERE p.group_id=g.id AND p.side=?1 AND p.representative=p.id AND p.proof IS NOT NULL AND p.hash IS NOT NULL AND NOT EXISTS(SELECT 1 FROM issues i WHERE i.id=p.id)))",[reference],|r|r.get(0))?;
            let missing_folder: bool = if self.review_folders_ready()? {
                self.db.query_row("SELECT EXISTS(SELECT 1 FROM folder_groups g WHERE NOT EXISTS(SELECT 1 FROM folder_nodes p WHERE p.id IN(g.a,g.b,g.c) AND p.side=?1 AND p.valid=1))",[reference],|r|r.get(0))?
            } else {
                false
            };
            if missing || missing_folder {
                out.reason = ReviewReason::MissingReference;
            }
        }
        Ok(out)
    }
    pub fn review_preview(&self) -> Result<ReviewSummary> {
        self.review_summary()
    }
}
