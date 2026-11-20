//! Explicit retry preserves an interrupted builder instead of overwriting it.
use super::*;

impl Repository {
    /// Reconcile a possible publication, then preserve a known interrupted
    /// stage in quarantine. This performs no source acquisition and grants no
    /// READY capability to a partial model. Unknown/corrupt/live stages refuse.
    pub fn prepare_publication_retry(
        &mut self,
        g: GenerationId,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.require_writer()?;
        self.require_allocation(g)?;
        cancelled(cancel)?;
        let stage = self
            .directory
            .join(format!(".stage-a-{:032x}-{:016x}", self.inventory.0, g.0));
        match std::fs::symlink_metadata(&stage) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.recover_catalogue(cancel);
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if metadata.is_symlink() || !metadata.is_dir() => {
                return Err(Error::Contract("unsafe interrupted stage preserved"));
            }
            Ok(_) => (),
        }
        // Keep the exclusive lock through the move, but close SQLite before
        // renaming a WAL group on Windows. Native-close uncertainty refuses.
        let store = self.validate_owned_group(&stage, cancel)?;
        #[cfg(any(windows, test))]
        let store = {
            crate::inventory::owner::install(&stage, &store._owner)?;
            // The first validation cannot survive a lease handoff. Reopen via the
            // verified external hard link and repeat ALL ownership/full checks.
            drop(release_store(store)?);
            cancelled(cancel)?;
            let store = self.validate_owned_group(&stage, cancel)?;
            crate::inventory::owner::verify_installed(&stage, &store._owner)?;
            store
        };
        let Store {
            db,
            _canonical,
            _memory,
            _owner,
            ..
        } = store;
        db.close().map_err(|(_, error)| Error::Sql(error))?;
        drop(_canonical);
        drop(_memory);
        cancelled(cancel)?;
        // Preflight the stage before explicit catalogue writes, so a future,
        // foreign or corrupt stage cannot trigger even pointer reconstruction.
        self.recover_catalogue(cancel)?;
        let bytes = lifecycle::artifact_bytes(&stage)?;
        let governor = Governor::global();
        let free = sortsize_core::resources::native_free_disk(&stage)
            .ok_or(Error::Contract("retry stage capacity unavailable"))?;
        // A live or sticky VFS ledger cannot be moved. Reacquiring a guard
        // succeeds only after all earlier ledger handles finished. Our own
        // exclusive native lease prevents a new cooperating opener in between.
        let closed = governor
            .guard_artifact_group(&stage, free, bytes)
            .ok_or(Error::Contract(
                "interrupted stage still owns native handles or quota",
            ))?;
        closed.finish(bytes);
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Contract("quarantine identifier clock unavailable"))?
            .as_nanos();
        let nonce = clock
            ^ (u128::from(std::process::id()) << 96)
            ^ u128::from(NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let quarantine = self.directory.join(format!(
            ".quarantine-a-{:032x}-{:016x}-{nonce:032x}",
            self.inventory.0, g.0
        ));
        if std::fs::symlink_metadata(&quarantine).is_ok() {
            return Err(Error::Contract(
                "quarantine destination occupied; preserved",
            ));
        }
        let moved = publish_directory(&stage, &quarantine);
        // A parent synchronization error after rename is not a rollback.
        // Relocate charges for the observed move even when synchronization fails.
        if !stage.exists()
            && quarantine.is_dir()
            && !governor.relocate_artifact_group(&stage, &quarantine)
        {
            return Err(Error::Contract(
                "quarantine moved; accounting reconciliation required",
            ));
        }
        moved?;
        drop(_owner);
        self.account()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
    #[test]
    fn orphan_anchor_is_preserved_without_bricking_a_certified_g0() {
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(1);
        let path = tmp.path().join("repo");
        let mut repo = Repository::open(&path, c.inventory, c.source).unwrap();
        let c = repo.next_context(c).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let mut source = repo.create_candidate(c, &mut plan).unwrap();
        let a = attempt_id(c.generation.0, 0, 1);
        source.begin_attempt(a).unwrap();
        batch(&mut source, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
        reconcile(&mut source, a);
        fold(&mut source, c.generation.0);
        source.seal(c).unwrap();
        let root = tmp.path().join("offline");
        assert!(
            repo.publish_with_plan(
                &source,
                c.generation,
                &root,
                32 * FRAME,
                &Default::default(),
                &mut plan
            )
            .unwrap()
            .committed()
        );
        drop(source);
        drop(plan);
        let db_before =
            std::fs::read(repo.generation_path(c.generation).join("inventory.db")).unwrap();
        // Model a crash after deleting an explicitly retired group but before
        // unlinking its physical anchor. Its zero-byte name remains valid.
        let retired = path.join(format!(
            ".stage-a-{:032x}-{:016x}",
            c.inventory.0, c.generation.0
        ));
        let retired_store =
            Store::create(&retired, c.inventory, c.source, Durability::Retained).unwrap();
        check_full(&retired_store.db).unwrap();
        crate::inventory::owner::install(&retired, &retired_store._owner).unwrap();
        drop(retired_store);
        for name in [
            "inventory.db",
            "inventory.db-wal",
            "inventory.db-shm",
            "owner.lock",
        ] {
            let file = retired.join(name);
            if file.exists() {
                std::fs::remove_file(file).unwrap();
            }
        }
        std::fs::remove_dir(&retired).unwrap();
        Governor::global().forget_deleted_artifact_group(&retired);
        let anchors = std::fs::read_dir(&path)
            .unwrap()
            .filter(|e| crate::inventory::owner::is_anchor_name(&e.as_ref().unwrap().file_name()))
            .count();
        assert_eq!(anchors, 1);
        drop(repo);
        let reader = Repository::open_readonly(&path).unwrap();
        let cap = reader
            .open_generation(c.generation, &Default::default())
            .unwrap();
        assert_eq!(cap.generation(), c.generation);
        drop(cap);
        drop(reader);
        let writer = Repository::open_existing(&path).unwrap();
        assert!(writer.forget(&Default::default()).is_err());
        assert_eq!(
            std::fs::read(
                path.join(format!("generation-{:016x}", c.generation.0))
                    .join("inventory.db")
            )
            .unwrap(),
            db_before
        );
        assert_eq!(
            std::fs::read_dir(&path)
                .unwrap()
                .filter(|e| crate::inventory::owner::is_anchor_name(
                    &e.as_ref().unwrap().file_name()
                ))
                .count(),
            1
        );
    }
    #[test]
    fn explicit_retry_quarantines_partial_model_and_publishes_from_same_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(1);
        let mut repository =
            Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        let c = repository.next_context(c).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let mut source = repository.create_candidate(c, &mut plan).unwrap();
        let a = attempt_id(c.generation.0, 0, 1);
        source.begin_attempt(a).unwrap();
        batch(&mut source, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
        reconcile(&mut source, a);
        fold(&mut source, c.generation.0);
        source.seal(c).unwrap();
        let root = tmp.path().join("offline");
        let stage = repository.directory.join(format!(
            ".stage-a-{:032x}-{:016x}",
            c.inventory.0, c.generation.0
        ));
        drop(plan);
        // Structurally valid, explicitly incomplete builder with our own core
        // and model definitions, as after a cancellation before READY commit.
        let mut interrupted =
            Store::create(&stage, c.inventory, c.source, Durability::Retained).unwrap();
        extraction::extract(&source, &mut interrupted, c.generation, &Default::default()).unwrap();
        interrupted.db.execute_batch(model::SQL).unwrap();
        drop(interrupted);
        repository
            .prepare_publication_retry(c.generation, &Default::default())
            .unwrap();
        assert!(!stage.exists());
        let quarantine = std::fs::read_dir(&repository.directory)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarantine-a-")
            })
            .unwrap();
        let before = std::fs::read(quarantine.join("inventory.db")).unwrap();
        assert!(
            CertifiedGeneration::open(&quarantine, c.generation, &root, &Default::default())
                .is_err()
        );
        let mut plan = Pipeline::for_publication().unwrap();
        assert!(
            repository
                .publish_with_plan(
                    &source,
                    c.generation,
                    &root,
                    16 * FRAME,
                    &Default::default(),
                    &mut plan
                )
                .unwrap()
                .committed()
        );
        assert_eq!(
            std::fs::read(quarantine.join("inventory.db")).unwrap(),
            before
        );
        assert_eq!(
            repository.latest_context().unwrap().unwrap().generation,
            c.generation
        );
    }
}
