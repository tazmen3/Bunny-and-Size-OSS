use super::*;
use std::path::PathBuf;
impl Store {
    /// Resolve a native relative navigation intention in this generation only.
    /// The deepest currently owned directory is returned when its descendants
    /// have not been observed yet. Sealed generations remain readable; this is
    /// not a writer capability. Indexed lookups do not read the source.
    pub fn navigation_directory(
        &self,
        c: AcquisitionContext,
        relative: &std::path::Path,
    ) -> Result<EntryId> {
        let _timer = Phase::NavigationDirectory.start();
        crate::scheduler::validate_navigation_path(relative)?;
        self.require_integrity()?;
        if c.inventory != self.inventory
            || c.source != self.source
            || self.acquisition_context(c.generation)? != c
        {
            return Err(Error::Contract("wrong navigation acquisition context"));
        }
        let mut parent = EntryId(0);
        let mut lookup = self.db.prepare(
            "SELECT e.id FROM entries e JOIN tasks p ON p.generation=e.generation AND p.directory=e.parent AND p.kind=0 WHERE e.generation=?1 AND e.parent=?2 AND e.name=?3 AND e.present=1 AND e.kind=2 AND e.attempt=p.attempt AND substr(p.key,1,length(?4))=?4",
        )?;
        for component in relative.components() {
            let name = super::queries::native_path(std::path::Path::new(component.as_os_str()));
            let child = lookup
                .query_row(
                    params![
                        unsigned(c.generation.0),
                        unsigned(parent.0),
                        name.encode(),
                        context(c)
                    ],
                    |row| number(row, 0).map(EntryId),
                )
                .optional()?;
            let Some(child) = child else {
                break;
            };
            parent = child;
        }
        Ok(parent)
    }
    pub(crate) fn reconcile_next(&mut self, c: AcquisitionContext) -> Result<bool> {
        let _timer = Phase::ReconcileDeferred.start();
        generation(&self.db, c)?;
        let row=self.db.query_row("SELECT directory,attempt,owner,branch FROM tasks WHERE generation=?1 AND kind=0 AND listing=1 AND reconciled=0 AND substr(key,1,length(?2))=?2 ORDER BY directory LIMIT 1",params![unsigned(c.generation.0),context(c)],|r|Ok((number(r,0)?,number(r,1)?,number(r,2)?,number(r,3)?))).optional()?;
        let Some((directory, attempt, owner, branch)) = row else {
            return Ok(true);
        };
        if !current_branch(&self.db, c.generation, EntryId(directory), true)? {
            let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
            let tx = self.db.transaction()?;
            tx.execute(
                "UPDATE tasks SET listing=2 WHERE generation=?1 AND directory=?2 AND kind=0",
                params![unsigned(c.generation.0), unsigned(directory)],
            )?;
            dirty(&tx, c.generation)?;
            metrics::commit(tx, Phase::ReconcileCommit)?;
            return Ok(false);
        }
        let a = AttemptKey {
            task: TaskKey {
                context: c,
                directory: EntryId(directory),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(attempt),
            owner: OwnerEpoch(owner),
            branch: BranchRevision(branch),
        };
        self.reconcile_step(a, ComparisonScope::Comparable, 512)?;
        Ok(false)
    }
    /// Only work whose parent was reobserved in the current context is admitted.
    /// Old unowned descendants cannot bypass root-first recovery.
    pub fn pending_directories(&self, c: AcquisitionContext, limit: usize) -> Result<Vec<EntryId>> {
        let _timer = Phase::PendingDirectories.start();
        generation(&self.db, c)?;
        let mut q=self.db.prepare("SELECT t.directory FROM tasks t JOIN entries e ON e.generation=t.generation AND e.id=t.directory LEFT JOIN tasks p ON p.generation=e.generation AND p.directory=e.parent AND p.kind=0 WHERE t.generation=?1 AND t.kind=0 AND t.key IS NULL AND e.present=1 AND e.kind=2 AND (e.parent IS NULL OR (e.attempt=p.attempt AND substr(p.key,1,length(?3))=?3)) ORDER BY t.directory LIMIT ?2")?;
        let trace_started = crate::scan_trace::enabled().then(std::time::Instant::now);
        let ids = (|| {
            q.query_map(
                params![unsigned(c.generation.0), limit.min(16) as i64, context(c)],
                |r| number(r, 0).map(EntryId),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
        })();
        crate::scan_trace::statement("pending_directories", &q, trace_started);
        Ok(ids?)
    }
    pub fn directory_pending(&self, c: AcquisitionContext, id: EntryId) -> Result<bool> {
        let _timer = Phase::DirectoryPending.start();
        generation(&self.db, c)?;
        let row: Option<(Option<u64>,bool,u64)> = self.db.query_row("SELECT e.parent,t.key IS NULL AND t.listing=0 AND e.present=1 AND e.kind=2,e.attempt FROM tasks t JOIN entries e ON e.generation=t.generation AND e.id=t.directory WHERE t.generation=?1 AND t.directory=?2 AND t.kind=0",params![unsigned(c.generation.0),unsigned(id.0)],|r|Ok((opt_number(r,0)?,r.get(1)?,number(r,2)?))).optional()?;
        let Some((parent, pending, seen)) = row else {
            return Ok(false);
        };
        if !pending {
            return Ok(false);
        }
        let Some(parent) = parent else {
            return Ok(id.0 == 0);
        };
        let a: Option<u64> = self.db.query_row(
            "SELECT attempt FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",
            params![unsigned(c.generation.0), unsigned(parent)],
            |r| opt_number(r, 0),
        )?;
        Ok(a == Some(seen) && current_branch(&self.db, c.generation, EntryId(parent), true)?)
    }
    /// Bounded temporary path reconstruction; never retained for the whole tree.
    pub fn relative_directory(&self, c: AcquisitionContext, mut id: EntryId) -> Result<PathBuf> {
        let _timer = Phase::RelativeDirectory.start();
        generation(&self.db, c)?;
        let mut names = Vec::new();
        let mut bytes = 0;
        while id.0 != 0 {
            let (parent,name):(u64,Vec<u8>)=self.db.query_row("SELECT parent,name FROM entries WHERE generation=?1 AND id=?2 AND present=1 AND kind=2",params![unsigned(c.generation.0),unsigned(id.0)],|r|Ok((number(r,0)?,r.get(1)?)))?;
            bytes += name.len();
            if bytes > 32 * 1024 || names.len() >= 1024 || parent >= id.0 {
                return Err(Error::Contract("native path budget or invalid ancestor"));
            }
            let n =
                NativeName::decode(&name).map_err(|_| Error::Contract("invalid native name"))?;
            names.push(crate::scheduler::native_name(&n)?);
            id = EntryId(parent);
        }
        let mut path = PathBuf::new();
        for name in names.into_iter().rev() {
            path.push(name);
        }
        Ok(path)
    }
}
