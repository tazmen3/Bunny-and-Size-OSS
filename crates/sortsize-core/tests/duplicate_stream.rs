use sortsize_core::{
    CancellationToken,
    acquisition::{Guard, Policy},
    duplicates::{
        DuplicateOptions,
        stream::{Proof, Verifier},
    },
};

struct Sources {
    _root: tempfile::TempDir,
    a: std::path::PathBuf,
    b: std::path::PathBuf,
    guard_a: Guard,
    guard_b: Guard,
}
impl Sources {
    fn new(a_bytes: &[u8], b_bytes: &[u8]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_a = root.path().join("source-a");
        let root_b = root.path().join("source-b");
        std::fs::create_dir(&root_a).unwrap();
        std::fs::create_dir(&root_b).unwrap();
        let a = root_a.join("original");
        let b = root_b.join("renamed-copy");
        std::fs::write(&a, a_bytes).unwrap();
        std::fs::write(&b, b_bytes).unwrap();
        let guard_a = Guard::open(&root_a, Policy::default()).unwrap();
        let guard_b = Guard::open(&root_b, Policy::default()).unwrap();
        Self {
            _root: root,
            a,
            b,
            guard_a,
            guard_b,
        }
    }
}

#[test]
fn disjoint_sources_compare_with_their_own_roots_and_one_shared_read_budget() {
    let s = Sources::new(b"same", b"same");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    assert_eq!(
        v.hash(&s.guard_a, &s.a, &pa).unwrap(),
        v.hash(&s.guard_b, &s.b, &pb).unwrap()
    );
    // A source guard never authorizes a member belonging to the other source.
    assert!(v.equal(&s.guard_a, &s.a, &pa, &s.b, &pb).is_err());
    assert!(v.equal(&s.guard_b, &s.a, &pa, &s.b, &pb).is_err());
    assert!(
        v.equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
            .unwrap()
    );
    assert_eq!(v.progress().files_hashed, 2);
    assert_eq!(v.progress().bytes_read, 16);
}

#[test]
fn equal_size_is_not_content_equality_across_sources() {
    let s = Sources::new(b"same", b"else");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    assert_ne!(
        v.hash(&s.guard_a, &s.a, &pa).unwrap(),
        v.hash(&s.guard_b, &s.b, &pb).unwrap()
    );
    assert!(
        !v.equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
            .unwrap()
    );
}

#[test]
fn stale_member_or_disconnected_root_invalidates_the_cross_source_proof() {
    let s = Sources::new(b"same", b"same");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    v.hash(&s.guard_a, &s.a, &pa).unwrap();
    v.hash(&s.guard_b, &s.b, &pb).unwrap();
    // Keep size equal and explicitly advance mtime. Two immediate writes
    // can share the same filesystem timestamp on Windows.
    let modified = std::fs::metadata(&s.b).unwrap().modified().unwrap();
    std::fs::write(&s.b, b"else").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&s.b)
        .unwrap()
        .set_modified(modified + std::time::Duration::from_secs(4))
        .unwrap();
    assert!(
        v.equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
            .is_err()
    );
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    let root_b = s.b.parent().unwrap();
    std::fs::rename(root_b, root_b.with_file_name("disconnected-b")).unwrap();
    std::fs::create_dir(root_b).unwrap();
    std::fs::write(&s.b, b"else").unwrap();
    assert!(
        v.equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
            .is_err()
    );
    // Source A's guard and proof remain valid independently of B's failure.
    v.validate(&s.guard_a, &s.a, &pa).unwrap();
}

#[test]
fn equal_bytes_that_no_longer_match_the_bucket_digest_are_rejected() {
    let s = Sources::new(b"same", b"same");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let hash = v.hash(&s.guard_a, &s.a, &pa).unwrap();

    std::fs::write(&s.a, b"else").unwrap();
    std::fs::write(&s.b, b"else").unwrap();
    // Current stamps model the case where the metadata checks accept both
    // members even though their equal bytes differ from the earlier bucket.
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    let error = v
        .equal_hashed_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb), &hash)
        .unwrap_err();
    assert_eq!(error.to_string(), "Content changed since hashing");
    assert_eq!(v.progress().bytes_read, 12);
}

#[test]
fn cross_source_comparison_stops_at_the_effective_global_budget() {
    let s = Sources::new(b"same", b"same");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(
        DuplicateOptions {
            max_bytes_read: Some(15),
            ..DuplicateOptions::default()
        },
        &cancel,
    );
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    v.hash(&s.guard_a, &s.a, &pa).unwrap();
    v.hash(&s.guard_b, &s.b, &pb).unwrap();
    let e = v
        .equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::QuotaExceeded);
    assert_eq!(v.progress().bytes_read, 15);
    assert_eq!(v.progress().files_hashed, 2);
}

#[test]
fn cancelled_cross_source_verification_reads_no_more_content_even_for_empty_files() {
    let s = Sources::new(b"", b"");
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 0, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 0, None).unwrap();
    cancel.cancel();
    let e = v
        .equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(v.progress().bytes_read, 0);
    assert_eq!(v.progress().files_hashed, 0);
}

#[test]
fn cross_root_hardlink_proofs_keep_the_same_native_identity() {
    let s = Sources::new(b"same", b"same");
    std::fs::remove_file(&s.b).unwrap();
    std::fs::hard_link(&s.a, &s.b).unwrap();
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&s.guard_a, &s.a, 4, None).unwrap();
    let pb = v.observe(&s.guard_b, &s.b, 4, None).unwrap();
    assert_eq!(pa.identity(), pb.identity());
    assert!(
        v.equal_between((&s.guard_a, &s.a, &pa), (&s.guard_b, &s.b, &pb))
            .unwrap()
    );
    // This is content equality, not evidence of two independent copies.
    assert_eq!(pa, pb);
}

#[test]
fn native_proof_roundtrip_sha_final_bytes_mutation_alias_and_cancellation() {
    let t = tempfile::tempdir().unwrap();
    let a = t.path().join("a");
    let b = t.path().join("b");
    std::fs::write(&a, b"same").unwrap();
    std::fs::write(&b, b"same").unwrap();
    let guard = Guard::open(t.path(), Policy::default()).unwrap();
    let cancel = CancellationToken::default();
    let mut v = Verifier::new(DuplicateOptions::default(), &cancel);
    let pa = v.observe(&guard, &a, 4, None).unwrap();
    let pb = v.observe(&guard, &b, 4, None).unwrap();
    assert_eq!(Proof::decode(&pa.encode().unwrap()).unwrap(), pa);
    assert_eq!(
        v.hash(&guard, &a, &pa).unwrap(),
        v.hash(&guard, &b, &pb).unwrap()
    );
    assert!(v.equal(&guard, &a, &pa, &b, &pb).unwrap());
    std::fs::remove_file(&b).unwrap();
    std::fs::hard_link(&a, &b).unwrap();
    assert!(v.validate(&guard, &b, &pb).is_err());
    let alias = v.observe(&guard, &b, 4, None).unwrap();
    assert_eq!(alias.identity(), pa.identity());
    std::fs::write(&a, b"changed").unwrap();
    assert!(v.hash(&guard, &a, &pa).is_err());
    cancel.cancel();
    assert!(v.observe(&guard, &a, 7, None).is_err());
    assert!(Proof::decode(&vec![0; 1025]).is_err());
}
