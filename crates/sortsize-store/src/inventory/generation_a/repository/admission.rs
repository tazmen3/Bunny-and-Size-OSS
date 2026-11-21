//! The old two ordinary / three protected logical generations bound spans
//! physical files; every copy is separately charged to the shared byte quota.
//! Allocation receipts are not observations and do not occupy a slot.
use super::*;
use std::collections::BTreeSet;

impl Repository {
    fn observation_ids(&self) -> Result<BTreeSet<u64>> {
        let mut ids = BTreeSet::new();
        for item in std::fs::read_dir(&self.directory)? {
            let item = item?;
            let name = item.file_name();
            let name = name
                .to_str()
                .ok_or(Error::Contract("foreign repository name"))?;
            let id = parse_name(name, "generation-", "")
                .or_else(|| parse_name(name, "candidate-", ""))
                .or_else(|| parse_name(name, "measurement-", ".json"));
            if let Some(id) = id {
                ids.insert(id);
            } else if let Some(stage) = name.strip_prefix(".stage-a-") {
                let prefix = format!("{:032x}-", self.inventory.0);
                let id = parse_name(stage, &prefix, "")
                    .ok_or(Error::Contract("foreign publication stage preserved"))?;
                ids.insert(id);
            }
            if ids.len() > 3 {
                return Err(Error::Contract(
                    "physical generation admission exceeded; preserve for explicit recovery",
                ));
            }
        }
        Ok(ids)
    }

    fn has_unpublished(&self, ids: &BTreeSet<u64>) -> Result<bool> {
        for id in ids {
            if self.published(GenerationId(*id))?.is_none() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Retire only certified, unleased observations already superseded by the
    /// current and last-complete references. A recoverable unpublished candidate
    /// is never silently discarded to start another acquisition.
    pub(super) fn admit_allocation(&mut self, fresh_acquisition: bool) -> Result<()> {
        self.require_writer()?;
        // A crash may leave the closed acquisition copy after a committed
        // publication. Reclaim it only after full certification and exact
        // canonical equality; an active native lease conservatively keeps it.
        for id in self.observation_ids()? {
            let g = GenerationId(id);
            if self.published(g)?.is_some() && self.candidate_path(g).exists() {
                match self.discard_candidate(g, &CancellationToken::default()) {
                    Ok(()) => (),
                    Err(Error::Contract("inventory already owned or native lock unavailable")) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let mut ids = self.observation_ids()?;
        if fresh_acquisition
            && let Some(latest) = self.current.latest
            && let Some(id) = ids.last().copied().filter(|id| *id > latest)
            && self.published(GenerationId(id))?.is_none()
            && self.quarantine_empty_candidate(GenerationId(id), &CancellationToken::default())?
        {
            ids.remove(&id);
        }
        if self.has_unpublished(&ids)? {
            // Reserving a new ID does not materialize another observation.
            // The separate creation guard still permits only one candidate.
            return if ids.len() < 3 {
                Ok(())
            } else {
                Err(Error::Contract(
                    "pending physical generations require explicit resume or recovery",
                ))
            };
        }
        for id in ids {
            if self.current.latest == Some(id) || self.current.last_complete == Some(id) {
                continue;
            }
            match self.purge(GenerationId(id), &CancellationToken::default()) {
                Ok(()) => (),
                // A reader may acquire its shared lease between enumeration and
                // retirement. Unavailable locks are conservatively protected;
                // every other error (including corruption) aborts admission.
                Err(Error::Contract("inventory already owned or native lock unavailable")) => (),
                Err(error) => return Err(error),
            }
        }
        if self.observation_ids()?.len() >= 3 {
            return Err(Error::Contract(
                "three protected physical generations; release a reference before acquisition",
            ));
        }
        Ok(())
    }

    pub(super) fn admit_candidate_creation(&self, g: GenerationId) -> Result<()> {
        let ids = self.observation_ids()?;
        if ids.contains(&g.0) {
            return Err(Error::Contract(
                "physical generation identity already materialized",
            ));
        }
        if self.has_unpublished(&ids)? || ids.len() >= 3 {
            return Err(Error::Contract(
                "physical candidate admission requires resume or reference retirement",
            ));
        }
        Ok(())
    }

    pub(super) fn admit_publication(&mut self, g: GenerationId) -> Result<()> {
        if !self.observation_ids()?.contains(&g.0) {
            self.admit_allocation(false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn measured(repo: &mut Repository, root: &Path, partial: bool) -> GenerationId {
        let c = repo.next_context(context_id(0)).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let mut source = repo.create_candidate(c, &mut plan).unwrap();
        let a = attempt_id(c.generation.0, 0, 1);
        source.begin_attempt(a).unwrap();
        let rows = if partial {
            vec![Observation {
                name: queries::native_path(Path::new("unlisted-child")),
                kind: Kind::Directory,
                logical: None,
                allocated: None,
                modified: None,
                metadata_known: true,
            }]
        } else {
            vec![]
        };
        batch(&mut source, a, 0, Some(ListingEnd::Exhausted), &rows).unwrap();
        reconcile(&mut source, a);
        fold(&mut source, c.generation.0);
        let coverage = source.seal(c).unwrap();
        assert_eq!(coverage == ScopeCoverage::Partial, partial);
        assert!(
            repo.publish_with_plan(
                &source,
                c.generation,
                root,
                16 * FRAME,
                &Default::default(),
                &mut plan
            )
            .unwrap()
            .committed()
        );
        drop(source);
        drop(plan);
        repo.discard_candidate(c.generation, &Default::default())
            .unwrap();
        c.generation
    }

    #[test]
    fn physical_admission_retires_superseded_ordinary_generations() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        let root = tmp.path().join("offline");
        let first = measured(&mut repo, &root, false);
        let second = measured(&mut repo, &root, false);
        assert_eq!(repo.observation_ids().unwrap().len(), 2);
        let third = measured(&mut repo, &root, false);
        assert_eq!(
            repo.observation_ids().unwrap(),
            BTreeSet::from([second.0, third.0])
        );
        assert!(!repo.generation_path(first).exists());
    }

    #[test]
    fn three_leased_references_block_fourth_without_reusing_ids_or_mutating_g0() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        let root = tmp.path().join("offline");
        let first = measured(&mut repo, &root, false);
        let first_cap = repo.open_generation(first, &Default::default()).unwrap();
        let before = std::fs::read(first_cap.directory().join("inventory.db")).unwrap();
        let second = measured(&mut repo, &root, false);
        let second_cap = repo.open_generation(second, &Default::default()).unwrap();
        let third = measured(&mut repo, &root, false);
        assert_eq!(repo.observation_ids().unwrap().len(), 3);
        assert!(repo.next_context(c).is_err());
        assert_eq!(
            std::fs::read(first_cap.directory().join("inventory.db")).unwrap(),
            before
        );
        drop(first_cap);
        let fourth = repo.next_context(c).unwrap();
        assert!(fourth.generation > third);
        let mut plan = Pipeline::plan().unwrap();
        let candidate = repo.create_candidate(fourth, &mut plan).unwrap();
        assert_eq!(repo.observation_ids().unwrap().len(), 3);
        assert!(!repo.generation_path(first).exists());
        assert!(repo.next_context(c).is_err());
        drop(candidate);
        drop(second_cap);
    }

    #[test]
    fn latest_partial_and_last_complete_protect_two_distinct_files() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        let root = tmp.path().join("offline");
        let complete = measured(&mut repo, &root, false);
        let partial = measured(&mut repo, &root, true);
        let third = repo.next_context(c).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let _candidate = repo.create_candidate(third, &mut plan).unwrap();
        assert_eq!(repo.observation_ids().unwrap().len(), 3);
        assert!(repo.purge(complete, &Default::default()).is_err());
        assert!(repo.purge(partial, &Default::default()).is_err());
        assert!(repo.next_context(c).is_err());
    }

    #[test]
    #[ignore = "spawned and terminated by the empty candidate recovery test"]
    fn empty_candidate_crash_child() {
        let marker = PathBuf::from(std::env::var_os("BUNNY_EMPTY_CANDIDATE_MARKER").unwrap());
        let path = PathBuf::from(std::env::var_os("BUNNY_EMPTY_CANDIDATE_REPO").unwrap());
        let mut repo = Repository::open_existing(&path).unwrap();
        let previous = repo.latest_context().unwrap().unwrap().context;
        let abandoned = repo.next_context(previous).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let mut candidate = repo.create_candidate_store(abandoned, &mut plan).unwrap();
        // Session initialization has upgraded the schema. No generation
        // or observation exists when the parent terminates this process.
        candidate.enable_properties().unwrap();
        std::fs::write(marker, abandoned.generation.0.to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
        drop(candidate);
        drop(repo);
    }

    #[test]
    fn killed_empty_candidate_releases_one_slot_and_keeps_both_references() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("repo");
        let root = tmp.path().join("offline");
        let c = context_id(0);
        let mut repo = Repository::open(&path, c.inventory, c.source).unwrap();
        let complete = measured(&mut repo, &root, false);
        let partial = measured(&mut repo, &root, true);
        let images = [complete, partial].map(|g| repo.generation_path(g).join("inventory.db"));
        let before = images.each_ref().map(|p| std::fs::read(p).unwrap());
        let pointer = std::fs::read(path.join("CURRENT.json")).unwrap();
        drop(repo);

        let marker = tmp.path().join("empty-ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "inventory::generation_a::repository::admission::tests::empty_candidate_crash_child",
            ])
            .env("BUNNY_EMPTY_CANDIDATE_MARKER", &marker)
            .env("BUNNY_EMPTY_CANDIDATE_REPO", &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !marker.exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("empty candidate child exited before the marker: {status}");
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("empty candidate child did not create its marker");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(Repository::open_existing(&path).is_err());
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());

        let mut repo = Repository::open_existing(&path).unwrap();
        let abandoned = GenerationId(partial.0 + 1);
        let candidate_path = repo.candidate_path(abandoned);
        let candidate = std::fs::read(candidate_path.join("inventory.db")).unwrap();
        let wal = std::fs::read(candidate_path.join("inventory.db-wal")).unwrap();
        let next = repo.next_context(c).unwrap();
        assert_eq!(next.generation.0, abandoned.0 + 1);
        assert!(!candidate_path.exists());
        assert!(repo.require_allocation(abandoned).is_ok());
        let prefix = format!(".quarantine-a-{:032x}-{:016x}-", c.inventory.0, abandoned.0);
        let quarantine = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read(quarantine.join("inventory.db")).unwrap(),
            candidate
        );
        assert_eq!(
            std::fs::read(quarantine.join("inventory.db-wal")).unwrap(),
            wal
        );
        assert_eq!(std::fs::read(path.join("CURRENT.json")).unwrap(), pointer);
        assert_eq!(images.each_ref().map(|p| std::fs::read(p).unwrap()), before);
        drop(repo);

        let repo = Repository::open_existing(&path).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let retry = repo.create_candidate(next, &mut plan).unwrap();
        assert_eq!(retry.acquisition_context(next.generation).unwrap(), next);
        assert!(repo.open_generation(complete, &Default::default()).is_ok());
        assert!(repo.open_generation(partial, &Default::default()).is_ok());
    }

    #[test]
    fn fresh_allocation_preserves_live_old_and_nonempty_candidates() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let mut repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        let root = tmp.path().join("offline");
        measured(&mut repo, &root, false);
        measured(&mut repo, &root, true);
        let third = repo.next_context(c).unwrap();
        let mut plan = Pipeline::plan().unwrap();
        let candidate = repo.create_candidate_store(third, &mut plan).unwrap();
        let path = repo.candidate_path(third.generation);
        assert!(repo.next_context(c).is_err(), "live candidate lease");
        drop(candidate);
        let before = std::fs::read(path.join("inventory.db")).unwrap();
        assert!(repo.next_context(c).is_err(), "supported older schema");
        assert_eq!(std::fs::read(path.join("inventory.db")).unwrap(), before);
        drop(plan);

        let mut candidate = Store::reopen(&path, Durability::Retained).unwrap();
        candidate.enable_properties().unwrap();
        candidate.create_generation(third).unwrap();
        drop(candidate);
        let before = std::fs::read(path.join("inventory.db")).unwrap();
        assert!(
            repo.next_context(c).is_err(),
            "installed generation without a scan"
        );
        assert_eq!(std::fs::read(path.join("inventory.db")).unwrap(), before);
        assert!(repo.published(third.generation).unwrap().is_none());
    }

    #[test]
    fn descriptor_catalogue_has_an_aggregate_byte_bound_before_the_row_bound() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        // Deliberately forged descriptors, never promoted to a capability. An
        // attacker-controlled catalogue must be bounded before certification.
        let root = tmp.path().join("r".repeat(10 * 1024));
        for id in 1..=64 {
            write_json(
                &repo.directory,
                &format!("measurement-{id:016x}.json"),
                &Measurement {
                    version: REPOSITORY_VERSION,
                    generation: id,
                    root: queries::native_path(&root).encode(),
                    context: context(context_id(id)),
                    complete: true,
                    started: 0,
                    measured: Some(1),
                },
                false,
            )
            .unwrap();
        }
        assert!(matches!(
            repo.descriptors(),
            Err(Error::Contract("repository catalogue budget"))
        ));
    }

    #[test]
    fn final_u64_generation_is_allocated_once_and_exhaustion_survives_restart() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(0);
        let path = tmp.path().join("repo");
        let mut repo = Repository::open(&path, c.inventory, c.source).unwrap();
        write_json(
            &path,
            "STATE.json",
            &State {
                version: REPOSITORY_VERSION,
                next_generation: u64::MAX,
                exhausted: false,
            },
            true,
        )
        .unwrap();
        let last = repo.next_context(c).unwrap();
        assert_eq!(last.generation.0, u64::MAX);
        let before = std::fs::read(path.join("STATE.json")).unwrap();
        assert!(repo.next_context(c).is_err());
        assert_eq!(std::fs::read(path.join("STATE.json")).unwrap(), before);
        drop(repo);
        let mut restarted = Repository::open_existing(&path).unwrap();
        assert!(restarted.next_context(c).is_err());
        assert_eq!(std::fs::read(path.join("STATE.json")).unwrap(), before);
    }

    #[test]
    fn legacy_maximum_generation_can_be_adopted_and_read_without_counter_overflow() {
        let _serial = SERIAL.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let c = context_id(u64::MAX);
        let mut legacy = Store::create(
            &tmp.path().join("legacy"),
            c.inventory,
            c.source,
            Durability::Retained,
        )
        .unwrap();
        legacy.create_generation(c).unwrap();
        let a = attempt_id(u64::MAX, 0, 1);
        legacy.begin_attempt(a).unwrap();
        batch(&mut legacy, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
        reconcile(&mut legacy, a);
        fold(&mut legacy, u64::MAX);
        legacy.seal(c).unwrap();
        let root = tmp.path().join("offline");
        let mut repo = Repository::open(&tmp.path().join("repo"), c.inventory, c.source).unwrap();
        assert!(
            repo.adopt_legacy(
                &legacy,
                c.generation,
                &root,
                16 * FRAME,
                &Default::default()
            )
            .unwrap()
            .committed()
        );
        let cap = repo
            .open_generation(c.generation, &Default::default())
            .unwrap();
        assert_eq!(cap.generation().0, u64::MAX);
        assert_eq!(cap.as_store().entry_count(c.generation).unwrap(), 1);
        assert!(repo.next_context(c).is_err());
        assert_eq!(legacy.entry_count(c.generation).unwrap(), 1);
    }
}
