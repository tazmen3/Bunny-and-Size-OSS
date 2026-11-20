//! Constant-size verifier shared with the bounded V0.5 disk grouping service.
//! Uses the same safe native content handles, SHA-256 and final byte comparison.
use super::*;
use crate::acquisition::Guard;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Proof {
    identity: Vec<u8>,
    length: u64,
    before: bool,
    seconds: u64,
    nanos: u32,
    change: (i64, i64),
}
impl Proof {
    fn from_stamp(s: Stamp) -> Self {
        let mut identity = Vec::with_capacity(25);
        match s.identity {
            ContentId::Native(crate::LocalFileId::Unix { device, inode }) => {
                identity.push(0);
                identity.extend(device.to_be_bytes());
                identity.extend(inode.to_be_bytes());
            }
            ContentId::Native(crate::LocalFileId::Windows {
                volume_serial,
                file_id,
            }) => {
                identity.push(1);
                identity.extend(volume_serial.to_be_bytes());
                identity.extend(file_id);
            }
            ContentId::Native(crate::LocalFileId::WindowsLegacy { volume, index }) => {
                identity.push(2);
                identity.extend(volume.to_be_bytes());
                identity.extend(index.to_be_bytes());
            }
            #[cfg(windows)]
            ContentId::WindowsLegacy { volume, index } => {
                identity.push(2);
                identity.extend(volume.to_be_bytes());
                identity.extend(index.to_be_bytes());
            }
        }
        let (before, d) = match s.modified.duration_since(UNIX_EPOCH) {
            Ok(d) => (false, d),
            Err(e) => (true, e.duration()),
        };
        Self {
            identity,
            length: s.length,
            before,
            seconds: d.as_secs(),
            nanos: d.subsec_nanos(),
            change: s.change,
        }
    }
    pub fn identity(&self) -> &[u8] {
        &self.identity
    }
    /// Temporary job record, not a persistent identity or public export format.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(io::Error::other)
    }
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let p: Self = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        let identity_valid = match p.identity.first() {
            Some(0) => p.identity.len() == 17,
            Some(1) => p.identity.len() == 25,
            Some(2) => p.identity.len() == 13 && p.identity[5..].iter().any(|b| *b != 0),
            _ => false,
        };
        if !identity_valid || p.nanos >= 1_000_000_000 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(p)
    }
}
pub struct Verifier<'a> {
    reader: Reader<'a>,
}
impl<'a> Verifier<'a> {
    pub fn new(options: DuplicateOptions, cancel: &'a CancellationToken) -> Self {
        Self {
            reader: Reader {
                cancel,
                options,
                progress: DuplicateProgress::default(),
            },
        }
    }
    pub fn progress(&self) -> DuplicateProgress {
        self.reader.progress
    }
    fn check_cancel(&self) -> io::Result<()> {
        if self.reader.cancel.is_cancelled() {
            Err(io::ErrorKind::Interrupted.into())
        } else {
            Ok(())
        }
    }
    fn open(&self, guard: &Guard, path: &Path, proof: Option<&Proof>) -> io::Result<(File, Stamp)> {
        self.check_cancel()?;
        if !guard.inspect(path)?.regular {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let f = content::open(path)?;
        let s = content::stamp(&f)?;
        if proof.is_some_and(|p| *p != Proof::from_stamp(s)) {
            return Err(io::Error::other("File changed since observation"));
        }
        Ok((f, s))
    }
    fn stable(&self, guard: &Guard, path: &Path, file: &File, stamp: Stamp) -> io::Result<()> {
        if content::stamp(file)? != stamp {
            return Err(io::Error::other("File changed while reading"));
        }
        self.open(guard, path, Some(&Proof::from_stamp(stamp)))?;
        Ok(())
    }
    pub fn observe(
        &self,
        guard: &Guard,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
    ) -> io::Result<Proof> {
        let (_, s) = self.open(guard, path, None)?;
        if s.length != length || modified.is_some_and(|m| m != s.modified) {
            return Err(io::Error::other("File changed since inventory"));
        }
        Ok(Proof::from_stamp(s))
    }
    /// Correlate the strict metadata observation with the guarded path and
    /// the handle used for content. Matching length and date alone would
    /// allow a different file to take the observed file's place.
    pub fn observe_acquired(
        &self,
        guard: &Guard,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
        matches: impl FnOnce(&crate::acquisition::Stamp) -> bool,
    ) -> io::Result<Proof> {
        self.check_cancel()?;
        let guarded = guard.inspect(path)?;
        if !guarded.regular || !matches(&guarded) {
            return Err(io::Error::other("File replaced since metadata acquisition"));
        }
        self.check_cancel()?;
        let file = content::open(path)?;
        let stamp = content::stamp(&file)?;
        #[cfg(not(windows))]
        let ContentId::Native(identity) = stamp.identity;
        #[cfg(windows)]
        let identity = match stamp.identity {
            ContentId::Native(id) => id,
            ContentId::WindowsLegacy { volume, index } => {
                crate::LocalFileId::WindowsLegacy { volume, index }
            }
        };
        if guarded.identity != Some(identity)
            || guarded.length != stamp.length
            || guarded
                .modified
                .is_some_and(|value| value != stamp.modified)
            || stamp.length != length
            || modified.is_some_and(|value| value != stamp.modified)
        {
            return Err(io::Error::other("File changed since metadata acquisition"));
        }
        Ok(Proof::from_stamp(stamp))
    }
    pub fn hash(&mut self, guard: &Guard, path: &Path, proof: &Proof) -> io::Result<[u8; 32]> {
        self.hash_with_progress(guard, path, proof, |_| {})
    }
    pub fn hash_with_progress(
        &mut self,
        guard: &Guard,
        path: &Path,
        proof: &Proof,
        mut notify: impl FnMut(DuplicateProgress),
    ) -> io::Result<[u8; 32]> {
        self.check_size(proof.length)?;
        let (mut f, s) = self.open(guard, path, Some(proof))?;
        let hash = self.reader.hash(&mut f, s.length, &mut notify)?;
        self.stable(guard, path, &f, s)?;
        self.reader.progress.files_hashed += 1;
        Ok(hash)
    }
    pub fn validate(&self, guard: &Guard, path: &Path, proof: &Proof) -> io::Result<()> {
        self.open(guard, path, Some(proof))?;
        Ok(())
    }
    pub fn equal(
        &mut self,
        guard: &Guard,
        a: &Path,
        pa: &Proof,
        b: &Path,
        pb: &Proof,
    ) -> io::Result<bool> {
        self.equal_between((guard, a, pa), (guard, b, pb))
    }
    /// Compare independently authorized members without broadening either
    /// source's root. Both reads share this verifier's global content budget.
    /// Each proof is checked before and after reading against its own guard;
    /// a stale or replaced member invalidates the comparison, never the root
    /// authorization of the other source.
    pub fn equal_between(
        &mut self,
        a: (&Guard, &Path, &Proof),
        b: (&Guard, &Path, &Proof),
    ) -> io::Result<bool> {
        self.compare(a, b, None, |_| {})
    }

    /// Check that equal bytes still match the digest used to form the bucket.
    /// The digest is computed from the comparison reads within the same budget.
    pub fn equal_hashed_between(
        &mut self,
        a: (&Guard, &Path, &Proof),
        b: (&Guard, &Path, &Proof),
        expected_hash: &[u8; 32],
    ) -> io::Result<bool> {
        self.equal_hashed_between_with_progress(a, b, expected_hash, |_| {})
    }

    pub fn equal_hashed_between_with_progress(
        &mut self,
        a: (&Guard, &Path, &Proof),
        b: (&Guard, &Path, &Proof),
        expected_hash: &[u8; 32],
        notify: impl FnMut(DuplicateProgress),
    ) -> io::Result<bool> {
        self.compare(a, b, Some(expected_hash), notify)
    }

    fn check_size(&self, length: u64) -> io::Result<()> {
        if let Some(limit) = self.reader.options.file_size_limit(length) {
            return Err(ContentLimit::FileSize(limit).io());
        }
        Ok(())
    }

    fn compare(
        &mut self,
        a: (&Guard, &Path, &Proof),
        b: (&Guard, &Path, &Proof),
        expected_hash: Option<&[u8; 32]>,
        mut notify: impl FnMut(DuplicateProgress),
    ) -> io::Result<bool> {
        let (guard_a, a, pa) = a;
        let (guard_b, b, pb) = b;
        if pa.length != pb.length {
            return Ok(false);
        }
        self.check_size(pa.length)?;
        let (mut aa, sa) = self.open(guard_a, a, Some(pa))?;
        let (mut bb, sb) = self.open(guard_b, b, Some(pb))?;
        let equal = self
            .reader
            .equal(&mut aa, &mut bb, sa.length, expected_hash, &mut notify)?;
        self.stable(guard_a, a, &aa, sa)?;
        self.stable(guard_b, b, &bb, sb)?;
        Ok(equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows read-only handles deny write sharing. On Linux a writer can
    // change an already opened file; inject it at the exact read notification
    // instead of relying on file size or scheduling a racing thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn mutation_during_cross_source_comparison_invalidates_the_proof() {
        use crate::acquisition::Policy;
        use std::{path::PathBuf, sync::Mutex};
        static MUTATE: Mutex<Option<PathBuf>> = Mutex::new(None);
        fn mutate_after_read(_: DuplicateProgress) {
            if let Some(path) = MUTATE.lock().unwrap().take() {
                std::fs::write(path, b"else").unwrap();
            }
        }

        let t = tempfile::tempdir().unwrap();
        let root_a = t.path().join("a");
        let root_b = t.path().join("b");
        std::fs::create_dir(&root_a).unwrap();
        std::fs::create_dir(&root_b).unwrap();
        let a = root_a.join("original");
        let b = root_b.join("copy");
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();
        let ga = Guard::open(&root_a, Policy::default()).unwrap();
        let gb = Guard::open(&root_b, Policy::default()).unwrap();
        let cancel = CancellationToken::default();
        let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
        let pa = v.observe(&ga, &a, 4, None).unwrap();
        let pb = v.observe(&gb, &b, 4, None).unwrap();
        assert_eq!(v.hash(&ga, &a, &pa).unwrap(), v.hash(&gb, &b, &pb).unwrap());
        *MUTATE.lock().unwrap() = Some(b.clone());
        let error = v.compare((&ga, &a, &pa), (&gb, &b, &pb), None, mutate_after_read);
        assert!(error.is_err(), "a mutation cannot be classified as unique");
        assert_eq!(std::fs::read(&b).unwrap(), b"else");
        assert_eq!(v.progress().bytes_read, 16);
        assert!(MUTATE.lock().unwrap().is_none());
    }

    #[test]
    fn windows_legacy_proof_roundtrip_keeps_the_native_namespace() {
        let legacy = Proof::from_stamp(Stamp {
            identity: ContentId::Native(crate::LocalFileId::WindowsLegacy {
                volume: 7,
                index: 9,
            }),
            length: 5,
            modified: UNIX_EPOCH,
            change: (0, 0),
        });
        let modern = Proof::from_stamp(Stamp {
            identity: ContentId::Native(crate::LocalFileId::Windows {
                volume_serial: 7,
                file_id: 9_u128.to_le_bytes(),
            }),
            length: 5,
            modified: UNIX_EPOCH,
            change: (0, 0),
        });
        assert_ne!(legacy.identity(), modern.identity());
        assert_eq!(Proof::decode(&legacy.encode().unwrap()).unwrap(), legacy);
        assert_eq!(Proof::decode(&modern.encode().unwrap()).unwrap(), modern);
        for identity in [
            vec![],
            vec![3; 13],
            vec![2; 12],
            vec![2; 14],
            [vec![2], vec![0; 12]].concat(),
        ] {
            let mut invalid = legacy.clone();
            invalid.identity = identity;
            assert!(Proof::decode(&invalid.encode().unwrap()).is_err());
        }
    }
}
