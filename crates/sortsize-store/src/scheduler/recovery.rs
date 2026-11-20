use super::*;

impl Session {
    /// Classify historical root continuity without granting persistent reuse.
    /// Explicit read-only consumers may retain an honest uncertainty state;
    /// resume and candidate acquisition still require `source_matches`.
    pub fn source_attachment(
        reference: &Store,
        generation: GenerationId,
        guard: &Guard,
    ) -> Result<sortsize_core::acquisition::Attachment> {
        let stored = reference.recorded_recovery_guard(generation)?;
        let stamp = guard.evidence().1;
        Ok(classify_attachment(
            stored.as_deref(),
            proof_boundary(guard),
            stamp.identity,
            stamp.created,
        ))
    }
    /// Compare the recorded source namespace with an already authorized child
    /// guard. This reads local proof bytes only; callers must additionally prove
    /// the selected child's recorded native identity and creation time.
    /// It does not authorize resume, inspect the parent, or widen the guard.
    pub(crate) fn subroot_boundary_attachment(
        reference: &Store,
        generation: GenerationId,
        recorded_root: &std::path::Path,
        guard: &Guard,
    ) -> Result<sortsize_core::acquisition::Attachment> {
        use sortsize_core::acquisition::Attachment;
        let stored = reference.recorded_recovery_guard(generation)?;
        let Some(recorded) = stored.as_deref().and_then(recorded_proof) else {
            return Ok(Attachment::Uncertain);
        };
        let Some(current) = proof_boundary_at(guard, recorded_root) else {
            return Ok(Attachment::Uncertain);
        };
        Ok(if current == recorded.boundary {
            Attachment::Compatible
        } else {
            Attachment::Different
        })
    }
    /// A recorded path is only a hint. Unknown or changed native continuity
    /// requires a separate inventory, even for a new explicit acquisition.
    pub fn source_matches(
        reference: &Store,
        generation: GenerationId,
        guard: &Guard,
    ) -> Result<bool> {
        Ok(Self::source_attachment(reference, generation, guard)?
            == sortsize_core::acquisition::Attachment::Compatible)
    }
    /// Read the work directory excluded from a recorded generation without
    /// opening either the source or the workspace.
    pub fn excluded_state_path(
        reference: &Store,
        generation: GenerationId,
    ) -> Result<Option<PathBuf>> {
        let stored = reference.recorded_recovery_guard(generation)?;
        let Some(scope) = stored
            .as_deref()
            .and_then(recorded_proof)
            .and_then(|proof| proof.scope)
        else {
            return Ok(None);
        };
        let length = u64::from_be_bytes(scope[2..10].try_into().unwrap()) as usize;
        Ok(Some(
            crate::inventory::queries::decode_recorded_path(&scope[10..10 + length])?.into_owned(),
        ))
    }

    pub fn resume_scope_matches(
        reference: &Store,
        generation: GenerationId,
        guard: &Guard,
    ) -> Result<bool> {
        match session_proof(guard) {
            Some(proof) => reference.compatible_guard(generation, &proof),
            None => Ok(false),
        }
    }
    /// A fresh physical candidate with the continuity and collection policy of
    /// a certified reference. The reference is never reopened for writing.
    pub fn verify_candidate(
        reference: &Store,
        reference_generation: GenerationId,
        candidate: Store,
        mut context: AcquisitionContext,
        guard: Guard,
    ) -> Result<Self> {
        let old = reference.acquisition_context(reference_generation)?;
        if old.inventory != context.inventory
            || old.source != context.source
            || context.generation.0 <= reference_generation.0
        {
            return Err(Error::Contract("physical verification identity mismatch"));
        }
        if !Self::source_matches(reference, reference_generation, &guard)? {
            return Err(Error::Contract(
                "source continuity changed; start a separate inventory",
            ));
        }
        let (allocation, owners) = reference
            .collection_options(reference_generation)?
            .unwrap_or((true, false));
        context.policy = observation_policy(ScanObservation::Strict);
        Self::new_with_options(
            candidate,
            context,
            guard.with_scan_observation(ScanObservation::Strict),
            CollectionOptions { allocation, owners },
        )
    }
    /// Explicit acquisition, never invoked by opening a dated inventory.
    /// Sealed generations stay immutable; an uncertain root requires a fresh
    /// candidate, not an absence sweep over the old candidate.
    pub fn resume(store: Store, generation: GenerationId, guard: Guard) -> Result<Self> {
        let c = store.acquisition_context(generation)?;
        let guard = guard.with_scan_observation(policy_observation(c.policy));
        Self::open_session(store, c, guard, true)
    }
    /// Explicit new measurement in a compatible retained inventory. Last sealed,
    /// last complete and selected reference stay protected during admission.
    pub fn verify(mut store: Store, reference: GenerationId, guard: Guard) -> Result<Self> {
        if !Self::source_matches(&store, reference, &guard)? {
            return Err(Error::Contract(
                "source continuity changed; start a separate inventory",
            ));
        }
        let records = store.saved_records()?;
        let old = store.acquisition_context(reference)?;
        let (allocation, owners) = store
            .collection_options(reference)?
            .unwrap_or((true, false));
        let next = records
            .iter()
            .map(|r| r.context.generation.0)
            .max()
            .unwrap_or(reference.0)
            .checked_add(1)
            .ok_or(Error::Contract("generation exhausted"))?;
        for r in &records {
            if r.context.generation != reference
                && store.latest_sealed()? != Some(r.context.generation)
                && store.latest_complete()? != Some(r.context.generation)
            {
                store.retire_generation(r.context.generation)?;
            }
        }
        let c = AcquisitionContext {
            policy: observation_policy(ScanObservation::Strict),
            generation: GenerationId(next),
            scan: ScanId(next),
            run: RunEpoch(next),
            mount: MountEpoch(next),
            validation: ValidationEpoch(next),
            ..old
        };
        Self::new_with_options(
            store,
            c,
            guard.with_scan_observation(ScanObservation::Strict),
            CollectionOptions { allocation, owners },
        )
    }
}

/// Versioned, bounded native equality proof. Includes creation time as well as
/// file identity; path/drive/inode alone never authorizes cross-process reuse.
/// Exact boundary equality is deliberately conservative after remount/reboot.
fn proof(guard: &Guard) -> Option<Vec<u8>> {
    let mut b = proof_boundary(guard)?;
    let stamp = guard.evidence().1;
    append_identity(&mut b, stamp.identity?)?;
    let (s, ns) = timestamp(stamp.created?)?;
    b.extend(s.to_be_bytes());
    b.extend(ns.to_be_bytes());
    (b.len() <= 128 * 1024).then_some(b)
}
pub(crate) fn workspace_identity(guard: &Guard) -> Option<Vec<u8>> {
    let mut bytes = b"bunny-workspace-v1\0".to_vec();
    let stamp = guard.evidence().1;
    append_identity(&mut bytes, stamp.identity?)?;
    let (seconds, nanos) = timestamp(stamp.created?)?;
    bytes.extend(seconds.to_be_bytes());
    bytes.extend(nanos.to_be_bytes());
    bytes.extend(encode_name(guard.root().as_os_str()).encode());
    (bytes.len() <= 128 * 1024).then_some(bytes)
}
pub(crate) fn session_proof(guard: &Guard) -> Option<Vec<u8>> {
    let mut bytes = proof(guard)?;
    if let Some(state) = guard.internal_state() {
        bytes.extend(proof(state)?);
    }
    (bytes.len() <= 128 * 1024).then_some(bytes)
}
/// Decode only the recorded exclusion; this does not authorize live I/O.
pub(crate) fn recorded_scope_path(bytes: &[u8]) -> Result<Option<PathBuf>> {
    let proof = recorded_proof(bytes).ok_or(Error::Contract("invalid recorded snapshot proof"))?;
    let Some(scope) = proof.scope else {
        return Ok(None);
    };
    let length = u64::from_be_bytes(scope[2..10].try_into().unwrap()) as usize;
    Ok(Some(
        crate::inventory::queries::decode_recorded_path(&scope[10..10 + length])?.into_owned(),
    ))
}
fn proof_boundary(guard: &Guard) -> Option<Vec<u8>> {
    proof_boundary_at(guard, guard.root())
}
fn proof_boundary_at(guard: &Guard, recorded_root: &std::path::Path) -> Option<Vec<u8>> {
    let (boundary, stamp) = guard.evidence();
    let mut b = vec![1, u8::from(guard.policy().same_volume)];
    fn field(b: &mut Vec<u8>, v: &[u8]) {
        b.extend((v.len() as u64).to_be_bytes());
        b.extend(v);
    }
    for p in [
        recorded_root,
        boundary.path.as_path(),
        boundary.root.as_path(),
    ] {
        field(&mut b, &encode_name(p.as_os_str()).encode());
    }
    field(&mut b, &boundary.token);
    b.push(match boundary.class {
        sortsize_core::acquisition::SourceClass::Local => 0,
        sortsize_core::acquisition::SourceClass::Remote => 1,
        sortsize_core::acquisition::SourceClass::Unknown => 2,
    });
    (b.len() <= 128 * 1024 && stamp.directory).then_some(b)
}

struct RecordedProof<'a> {
    boundary: &'a [u8],
    prefix: &'a [u8],
    source: &'a [u8],
    scope: Option<&'a [u8]>,
}
fn recorded_proof(bytes: &[u8]) -> Option<RecordedProof<'_>> {
    let mut source = proof_prefix(bytes)?;
    let suffix = bytes.get(source.source.len()..)?;
    if !suffix.is_empty() {
        if proof_prefix(suffix)?.source.len() != suffix.len() {
            return None;
        }
        source.scope = Some(suffix);
    }
    Some(source)
}
fn proof_prefix(bytes: &[u8]) -> Option<RecordedProof<'_>> {
    if bytes.len() > 128 * 1024 || bytes.first() != Some(&1) || *bytes.get(1)? > 1 {
        return None;
    }
    let mut offset = 2usize;
    for _ in 0..4 {
        let end = offset.checked_add(8)?;
        let length =
            usize::try_from(u64::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?)).ok()?;
        offset = end.checked_add(length)?;
        bytes.get(..offset)?;
    }
    if *bytes.get(offset)? > 2 {
        return None;
    }
    offset += 1;
    let boundary = bytes.get(..offset)?;
    let identity_length = match bytes.get(offset)? {
        0 => 17,
        1 => 25,
        2 => {
            if bytes.get(offset + 5..offset + 13)?.iter().all(|b| *b == 0) {
                return None;
            }
            13
        }
        _ => return None,
    };
    let prefix_end = offset.checked_add(identity_length)?;
    let end = prefix_end.checked_add(12)?;
    if u32::from_be_bytes(bytes.get(prefix_end + 8..end)?.try_into().ok()?) >= 1_000_000_000 {
        return None;
    }
    Some(RecordedProof {
        boundary,
        prefix: bytes.get(..prefix_end)?,
        source: bytes.get(..end)?,
        scope: None,
    })
}
fn classify_attachment(
    stored: Option<&[u8]>,
    boundary: Option<Vec<u8>>,
    identity: Option<sortsize_core::LocalFileId>,
    created: Option<std::time::SystemTime>,
) -> sortsize_core::acquisition::Attachment {
    use sortsize_core::acquisition::Attachment;
    let Some(recorded) = stored.and_then(recorded_proof) else {
        return Attachment::Uncertain;
    };
    let Some(mut current) = boundary else {
        return Attachment::Uncertain;
    };
    if current != recorded.boundary {
        return Attachment::Different;
    }
    let Some(identity) = identity else {
        return Attachment::Uncertain;
    };
    if append_identity(&mut current, identity).is_none() {
        return Attachment::Uncertain;
    }
    if current != recorded.prefix {
        return Attachment::Different;
    }
    let Some((seconds, nanos)) = created.and_then(timestamp) else {
        return Attachment::Uncertain;
    };
    current.extend(seconds.to_be_bytes());
    current.extend(nanos.to_be_bytes());
    if recorded.source == current {
        Attachment::Compatible
    } else {
        Attachment::Different
    }
}
fn append_identity(b: &mut Vec<u8>, identity: sortsize_core::LocalFileId) -> Option<()> {
    match identity {
        sortsize_core::LocalFileId::Unix { device, inode } => {
            b.push(0);
            b.extend(device.to_be_bytes());
            b.extend(inode.to_be_bytes());
        }
        sortsize_core::LocalFileId::Windows {
            volume_serial,
            file_id,
        } => {
            b.push(1);
            b.extend(volume_serial.to_be_bytes());
            b.extend(file_id);
        }
        sortsize_core::LocalFileId::WindowsLegacy { volume, index } if index != 0 => {
            b.push(2);
            b.extend(volume.to_be_bytes());
            b.extend(index.to_be_bytes());
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Durability;
    use sortsize_core::acquisition::Policy;
    #[test]
    fn attachment_without_birth_time_distinguishes_unknown_from_known_difference() {
        use sortsize_core::{LocalFileId, acquisition::Attachment};
        let t = tempfile::tempdir().unwrap();
        let guard = Guard::open(t.path(), Policy::default()).unwrap();
        let boundary = proof_boundary(&guard).unwrap();
        let identity = guard.evidence().1.identity.unwrap();
        let created = std::time::UNIX_EPOCH + Duration::from_secs(10);
        let mut stored = boundary.clone();
        append_identity(&mut stored, identity).unwrap();
        stored.extend(10_i64.to_be_bytes());
        stored.extend(0_u32.to_be_bytes());
        fn check(
            saved: Option<&[u8]>,
            boundary: Vec<u8>,
            identity: Option<LocalFileId>,
            created: Option<std::time::SystemTime>,
        ) -> Attachment {
            classify_attachment(saved, Some(boundary), identity, created)
        }
        assert_eq!(
            check(
                Some(stored.as_slice()),
                boundary.clone(),
                Some(identity),
                Some(created)
            ),
            Attachment::Compatible
        );
        assert_eq!(
            check(
                Some(stored.as_slice()),
                boundary.clone(),
                Some(identity),
                None
            ),
            Attachment::Uncertain
        );
        assert_eq!(
            check(None, boundary.clone(), Some(identity), Some(created)),
            Attachment::Uncertain
        );
        assert_eq!(
            check(Some(stored.as_slice()), boundary.clone(), None, None),
            Attachment::Uncertain
        );
        assert_eq!(
            check(
                Some(stored.as_slice()),
                boundary.clone(),
                Some(identity),
                Some(created + Duration::from_secs(1))
            ),
            Attachment::Different
        );
        let different_identity = match identity {
            LocalFileId::Unix { device, inode } => LocalFileId::Unix {
                device,
                inode: inode ^ 1,
            },
            LocalFileId::Windows {
                volume_serial,
                mut file_id,
            } => {
                file_id[0] ^= 1;
                LocalFileId::Windows {
                    volume_serial,
                    file_id,
                }
            }
            LocalFileId::WindowsLegacy { volume, index } => LocalFileId::WindowsLegacy {
                volume,
                index: if index == 1 { 2 } else { 1 },
            },
            _ => panic!("fixture requires a known native identity namespace"),
        };
        assert_eq!(
            check(
                Some(stored.as_slice()),
                boundary.clone(),
                Some(different_identity),
                None
            ),
            Attachment::Different
        );
        let mut changed_boundary = boundary.clone();
        // Change one valid native boundary-class tag, leaving its framing valid.
        *changed_boundary.last_mut().unwrap() = (boundary.last().unwrap() + 1) % 3;
        assert_eq!(
            check(Some(stored.as_slice()), changed_boundary, None, None),
            Attachment::Different
        );
        for malformed in [
            vec![],
            vec![1],
            stored[..stored.len() - 1].to_vec(),
            vec![0; 131073],
        ] {
            assert_eq!(
                check(
                    Some(malformed.as_slice()),
                    boundary.clone(),
                    Some(identity),
                    Some(created)
                ),
                Attachment::Uncertain
            );
        }
    }

    #[test]
    fn proof1_wire_encoding_remains_identical_to_the_previous_writer() {
        // This is the previous complete proof1 writer kept as a compatibility
        // oracle. The attachment classifier must not introduce a new cache or
        // silently change evidence already present in retained inventories.
        fn previous_writer(guard: &Guard) -> Option<Vec<u8>> {
            let (boundary, stamp) = guard.evidence();
            let mut bytes = vec![1, u8::from(guard.policy().same_volume)];
            for value in [
                encode_name(guard.root().as_os_str()).encode(),
                encode_name(boundary.path.as_os_str()).encode(),
                encode_name(boundary.root.as_os_str()).encode(),
                boundary.token.clone(),
            ] {
                bytes.extend((value.len() as u64).to_be_bytes());
                bytes.extend(value);
            }
            bytes.push(match boundary.class {
                sortsize_core::acquisition::SourceClass::Local => 0,
                sortsize_core::acquisition::SourceClass::Remote => 1,
                sortsize_core::acquisition::SourceClass::Unknown => 2,
            });
            append_identity(&mut bytes, stamp.identity?)?;
            let (seconds, nanos) = timestamp(stamp.created?)?;
            bytes.extend(seconds.to_be_bytes());
            bytes.extend(nanos.to_be_bytes());
            (bytes.len() <= 128 * 1024 && stamp.directory).then_some(bytes)
        }
        let t = tempfile::tempdir().unwrap();
        for same_volume in [false, true] {
            let guard = Guard::open(
                t.path(),
                Policy {
                    same_volume,
                    explicit_root: false,
                },
            )
            .unwrap();
            assert_eq!(proof(&guard), previous_writer(&guard));
        }
    }

    #[test]
    fn windows_legacy_recovery_identity_never_matches_padded_file_id() {
        use sortsize_core::LocalFileId;
        let mut legacy = Vec::new();
        let mut modern = Vec::new();
        append_identity(
            &mut legacy,
            LocalFileId::WindowsLegacy {
                volume: 7,
                index: 9,
            },
        )
        .unwrap();
        append_identity(
            &mut modern,
            LocalFileId::Windows {
                volume_serial: 7,
                file_id: 9_u128.to_le_bytes(),
            },
        )
        .unwrap();
        assert_eq!(legacy.len(), 13);
        assert_eq!(modern.len(), 25);
        assert_ne!(legacy, modern);
        assert!(
            append_identity(
                &mut Vec::new(),
                LocalFileId::WindowsLegacy {
                    volume: 7,
                    index: 0
                }
            )
            .is_none()
        );
    }
    fn run(s: &mut Session) {
        let start = Instant::now();
        while s.tick().unwrap().state != State::Idle {
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn native_resume_reenumerates_completed_branches_and_rejects_another_root() {
        let _serial = crate::scheduler::tests::TEST_LOCK.lock().unwrap();
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("a"), b"123").unwrap();
        fs::write(root.join("nested/b"), b"12345").unwrap();
        let c = AcquisitionContext {
            inventory: InventoryId(80),
            source: SourceId(81),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let path = t.path().join("store");
        let mut s = Session::new(
            Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap(),
            c,
            Guard::open(&root, Policy::default()).unwrap(),
        )
        .unwrap();
        run(&mut s);
        assert_eq!(s.store().entry_count(c.generation).unwrap(), 4);
        drop(s);
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(
            Session::resume(
                Store::reopen(&path, Durability::Retained).unwrap(),
                c.generation,
                Guard::open(&other, Policy::default()).unwrap()
            )
            .is_err()
        );
        fs::remove_file(root.join("a")).unwrap();
        fs::write(root.join("nested/b"), b"changed content").unwrap();
        fs::write(root.join("nested/new"), b"4").unwrap();
        let mut s = Session::resume(
            Store::reopen(&path, Durability::Retained).unwrap(),
            c.generation,
            Guard::open(&root, Policy::default()).unwrap(),
        )
        .unwrap();
        assert!(s.context().run > c.run);
        run(&mut s);
        assert!(s.status().quanta >= 2);
        let done = loop {
            if let Some(c) = s.finalize_step().unwrap() {
                break c;
            }
        };
        assert_eq!(done, ScopeCoverage::Complete);
        assert_eq!(s.store().totals(c.generation).unwrap().logical, Some(16));
        assert!(
            !s.store()
                .lookup(c.generation, EntryId(0), &encode_name(OsStr::new("a")))
                .unwrap()
                .unwrap()
                .present
        );
        eprintln!(
            "native root and completed descendant reobserved; removal/new/changed bytes exact"
        );
    }

    #[test]
    fn physical_verification_preserves_policy_and_refuses_changed_source() {
        let _serial = crate::scheduler::tests::TEST_LOCK.lock().unwrap();
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("source");
        let other = t.path().join("other");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(root.join("file"), b"reference").unwrap();
        let c = AcquisitionContext {
            inventory: InventoryId(180),
            source: SourceId(181),
            generation: GenerationId(1),
            scan: ScanId(1),
            policy: PolicyId(1),
            mount: MountEpoch(1),
            run: RunEpoch(1),
            validation: ValidationEpoch(1),
        };
        let mut first = Session::new_with_options(
            Store::create(
                &t.path().join("g0"),
                c.inventory,
                c.source,
                Durability::Retained,
            )
            .unwrap(),
            c,
            Guard::open(&root, Policy::default()).unwrap(),
            CollectionOptions {
                allocation: false,
                owners: true,
            },
        )
        .unwrap();
        run(&mut first);
        while first.finalize_step().unwrap().is_none() {}
        // End the scheduler acquisition, retain only the independent reference.
        let reference_path = first.store().directory().to_owned();
        drop(first);
        let reference = Store::reopen(&reference_path, Durability::Retained).unwrap();
        assert!(
            Session::source_matches(
                &reference,
                c.generation,
                &Guard::open(&root, Policy::default()).unwrap()
            )
            .unwrap()
        );
        assert!(
            !Session::source_matches(
                &reference,
                c.generation,
                &Guard::open(&other, Policy::default()).unwrap()
            )
            .unwrap()
        );
        let next = AcquisitionContext {
            generation: GenerationId(2),
            ..c
        };
        let candidate = || {
            Store::create(
                &t.path().join("g1"),
                c.inventory,
                c.source,
                Durability::Retained,
            )
            .unwrap()
        };
        let failed = Session::verify_candidate(
            &reference,
            c.generation,
            candidate(),
            next,
            Guard::open(&other, Policy::default()).unwrap(),
        );
        assert!(failed.is_err());
        let candidate = Store::reopen(&t.path().join("g1"), Durability::Retained).unwrap();
        fs::write(root.join("file"), b"new").unwrap();
        let mut second = Session::verify_candidate(
            &reference,
            c.generation,
            candidate,
            next,
            Guard::open(&root, Policy::default()).unwrap(),
        )
        .unwrap();
        run(&mut second);
        while second.finalize_step().unwrap().is_none() {}
        assert_eq!(reference.totals(c.generation).unwrap().logical, Some(9));
        assert_eq!(
            second.store().totals(next.generation).unwrap().logical,
            Some(3)
        );
        assert_eq!(
            second.store().collection_options(next.generation).unwrap(),
            Some((false, true))
        );
        drop(second);
        fs::rename(&root, t.path().join("old-source")).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(
            !Session::source_matches(
                &reference,
                c.generation,
                &Guard::open(&root, Policy::default()).unwrap()
            )
            .unwrap()
        );
    }
}
