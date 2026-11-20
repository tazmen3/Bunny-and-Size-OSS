use super::*;

#[cfg(test)]
mod tests;

impl Store {
    /// Local metadata only. A context is not a source authorization.
    pub fn acquisition_context(&self, g: GenerationId) -> Result<AcquisitionContext> {
        let b: Vec<u8> = self.db.query_row(
            "SELECT substr(context,1,82) FROM generations WHERE id=?1",
            [unsigned(g.0)],
            |r| r.get(0),
        )?;
        if b.len() != 81 || b[0] != 1 {
            return Err(Error::Contract("invalid context codec"));
        }
        let n = |offset| u64::from_be_bytes(b[offset..offset + 8].try_into().unwrap());
        Ok(AcquisitionContext {
            inventory: InventoryId(u128::from_be_bytes(b[1..17].try_into().unwrap())),
            source: SourceId(u128::from_be_bytes(b[17..33].try_into().unwrap())),
            generation: GenerationId(n(33)),
            scan: ScanId(n(41)),
            policy: PolicyId(n(49)),
            mount: MountEpoch(n(57)),
            run: RunEpoch(n(65)),
            validation: ValidationEpoch(n(73)),
        })
    }
    /// Called only after a new Guard and the process root lease are acquired.
    /// The SQLite owner lock excludes other processes. One bounded transaction
    /// fences every old task by changing the context; no mass UPDATE or offset.
    pub(crate) fn recover_candidate(
        &mut self,
        old: AcquisitionContext,
        proof: &[u8],
    ) -> Result<AcquisitionContext> {
        generation(&self.db, old)?;
        let version: i64 = self.db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(2..=6).contains(&version) {
            return Err(Error::Contract("legacy candidate has no recovery proof"));
        }
        let saved: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT proof FROM recovery_guard WHERE generation=?1",
                [unsigned(old.generation.0)],
                |r| r.get(0),
            )
            .optional()?;
        if saved.as_deref() != Some(proof) {
            return Err(Error::Contract(
                "source continuity is different or uncertain",
            ));
        }
        let next = |n: u64| {
            n.checked_add(1)
                .ok_or(Error::Contract("recovery epoch exhausted"))
        };
        let c = AcquisitionContext {
            scan: ScanId(next(old.scan.0)?),
            mount: MountEpoch(next(old.mount.0)?),
            run: RunEpoch(next(old.run.0)?),
            validation: ValidationEpoch(next(old.validation.0)?),
            ..old
        };
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE generations SET context=?2,coverage=0 WHERE id=?1",
            params![unsigned(c.generation.0), context(c)],
        )?;
        tx.execute("UPDATE tasks SET key=NULL,listing=0,reconciled=0,sweep_cursor=NULL,next_sequence=?2 WHERE generation=?1 AND directory=?2 AND kind=0",params![unsigned(c.generation.0),unsigned(0)])?;
        dirty(&tx, c.generation)?;
        tx.commit()?;
        Ok(c)
    }
    pub(crate) fn compatible_guard(&self, g: GenerationId, proof: &[u8]) -> Result<bool> {
        let stored: Option<Vec<u8>> = self
            .db
            .query_row(
                "SELECT proof FROM recovery_guard WHERE generation=?1",
                [unsigned(g.0)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(proof))
    }
    /// Read existing bounded evidence only. Legacy inventories and generations
    /// recorded without a birth time have no durable continuity proof.
    pub(crate) fn recorded_recovery_guard(&self, g: GenerationId) -> Result<Option<Vec<u8>>> {
        let version: i64 = self
            .db
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 2 {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT proof FROM recovery_guard WHERE generation=?1 AND length(proof)<=131072",
                [unsigned(g.0)],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub(crate) fn next_attempt(&self, c: AcquisitionContext, id: EntryId) -> Result<AttemptKey> {
        let _timer = Phase::NextAttempt.start();
        generation(&self.db, c)?;
        let (a,o,b):(Option<u64>,Option<u64>,Option<u64>)=self.db.query_row("SELECT attempt,owner,branch FROM tasks WHERE generation=?1 AND directory=?2 AND kind=0",params![unsigned(c.generation.0),unsigned(id.0)],|r|Ok((opt_number(r,0)?,opt_number(r,1)?,opt_number(r,2)?)))?;
        let next = |n: u64| {
            n.checked_add(1)
                .ok_or(Error::Contract("task epoch exhausted"))
        };
        Ok(AttemptKey {
            task: TaskKey {
                context: c,
                directory: id,
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(next(a.unwrap_or(0))?),
            owner: OwnerEpoch(next(o.unwrap_or(0).max(c.run.0))?),
            branch: BranchRevision(next(b.unwrap_or(0))?),
        })
    }
}
