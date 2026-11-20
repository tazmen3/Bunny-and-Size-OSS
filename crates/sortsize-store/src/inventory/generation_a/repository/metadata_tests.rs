use super::*;
use crate::inventory::tests::{attempt_id, batch, context_id, fold, reconcile};
use sortsize_core::resources::MemoryReading;
use std::sync::Mutex;
static SERIAL: Mutex<()> = Mutex::new(());

// A recorded label only. No filesystem call is made on this impossible source.
fn maximum_root(suffix: char) -> PathBuf {
    #[cfg(windows)]
    let prefix = "Z:\\";
    #[cfg(not(windows))]
    let prefix = "/";
    let mut root = prefix.to_owned();
    root.extend(std::iter::repeat_n('z', 32 * 1024 - prefix.len() - 1));
    root.push(suffix);
    let root = PathBuf::from(root);
    assert!(root.is_absolute());
    assert_eq!(root.as_os_str().as_encoded_bytes().len(), 32 * 1024);
    root
}
fn measured(source: &mut Store, g: u64, root: &Path) {
    let c = context_id(g);
    source.create_generation(c).unwrap();
    source.record_acquisition(c, root, 10 * g as i64).unwrap();
    let a = attempt_id(g, 0, 1);
    source.begin_attempt(a).unwrap();
    batch(source, a, 0, Some(ListingEnd::Exhausted), &[]).unwrap();
    reconcile(source, a);
    fold(source, g);
    source.seal(c).unwrap();
    source
        .record_measurement(c.generation, root, 10 * g as i64, 10 * g as i64 + 1)
        .unwrap();
}
#[test]
fn maximum_native_root_survives_publication_reopen_and_independent_copy() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let root = maximum_root('a');
    let c = context_id(1);
    let mut source = Store::create(
        &temp.path().join("source"),
        c.inventory,
        c.source,
        Durability::Retained,
    )
    .unwrap();
    measured(&mut source, 1, &root);
    let mut repo = Repository::open(&temp.path().join("repo"), c.inventory, c.source).unwrap();
    assert!(
        repo.adopt_legacy(
            &source,
            c.generation,
            &root,
            16 * FRAME,
            &Default::default()
        )
        .unwrap()
        .committed()
    );
    let document = repo.directory().join("measurement-0000000000000001.json");
    assert!(std::fs::metadata(&document).unwrap().len() > 128 * 1024);
    let published = repo.published(c.generation).unwrap().unwrap();
    assert_eq!(published.root, root);
    assert_eq!(
        repo.open_generation(c.generation, &Default::default())
            .unwrap()
            .root(),
        root
    );
    let copy_parent = temp.path().join("copies");
    lifecycle::ensure_root(&copy_parent).unwrap();
    let copied = repo
        .copy_to(&copy_parent.join("copy"), &Default::default())
        .unwrap();
    let copied_descriptor = copied.published(c.generation).unwrap().unwrap();
    assert_eq!(copied_descriptor.root, root);
    assert_eq!(copied_descriptor.context, c);
    assert_eq!(
        copied
            .open_generation(c.generation, &Default::default())
            .unwrap()
            .root(),
        root
    );
}
#[derive(Deserialize)]
struct OriginView {
    directory: Vec<u8>,
    generations: Vec<u64>,
    anchors: Vec<AnchorView>,
}
#[derive(Deserialize)]
struct AnchorView {
    root: Vec<u8>,
    context: Vec<u8>,
    started: i64,
    measured: Option<i64>,
}
#[test]
fn origin_three_maximum_native_roots_roundtrip_without_source_io() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let c = context_id(1);
    let path = temp.path().join("legacy");
    let mut source = Store::create(&path, c.inventory, c.source, Durability::Retained).unwrap();
    let roots = [maximum_root('a'), maximum_root('b'), maximum_root('c')];
    for (i, root) in roots.iter().enumerate() {
        measured(&mut source, i as u64 + 1, root);
    }
    drop(source);
    let source = Store::certify_readonly(&path, &Default::default()).unwrap();
    let mut repo = Repository::open(
        &Repository::companion_path(&path).unwrap(),
        c.inventory,
        c.source,
    )
    .unwrap();
    repo.bind_legacy_origin(&source).unwrap();
    let origin_path = repo.directory().join("ORIGIN.json");
    let length = std::fs::metadata(&origin_path).unwrap().len();
    assert!(length > 3 * 128 * 1024 && length < DOCUMENT_LIMIT);
    let origin: OriginView = read_json(&origin_path).unwrap();
    assert_eq!(decode_path(&origin.directory).unwrap(), source.directory());
    assert_eq!(origin.generations, vec![1, 2, 3]);
    assert_eq!(origin.anchors.len(), 3);
    for (i, anchor) in origin.anchors.iter().enumerate() {
        assert_eq!(decode_path(&anchor.root).unwrap(), roots[i]);
        assert_eq!(
            decode_context(&anchor.context).unwrap(),
            context_id(i as u64 + 1)
        );
        assert_eq!(anchor.started, 10 * (i as i64 + 1));
        assert_eq!(anchor.measured, Some(10 * (i as i64 + 1) + 1));
    }
    let before = std::fs::read(&origin_path).unwrap();
    repo.bind_legacy_origin(&source).unwrap();
    assert_eq!(before, std::fs::read(&origin_path).unwrap());
}
fn governor() -> Governor {
    Governor::new(
        MemoryMode::Auto,
        Some(MemoryReading {
            total: 8 * 1024 * FRAME,
            available: 8 * 1024 * FRAME,
        }),
        1024 * FRAME,
    )
}
#[test]
fn json_document_admission_precedes_decode_and_never_raises_global_limits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("document");
    std::fs::write(&path, b"[not valid JSON]").unwrap();
    let governor = governor();
    let limits = governor.usage().limits;
    let transport = governor
        .reserve(
            Category::Transport,
            limits.categories[Category::Transport as usize],
            None,
        )
        .unwrap();
    let margin = governor
        .reserve(
            Category::Margin,
            limits.categories[Category::Margin as usize],
            None,
        )
        .unwrap();
    assert!(matches!(
        read_json_in::<Identity>(&path, &governor),
        Err(Error::Contract("repository document memory admission"))
    ));
    drop((transport, margin));
    assert!(matches!(
        read_json_in::<Identity>(&path, &governor),
        Err(Error::Contract(
            "repository document invalid; explicit recovery required"
        ))
    ));
    assert_eq!(governor.usage().categories, [0; 6]);
    assert_eq!(governor.usage().limits.bytes, limits.bytes);
}
#[test]
fn oversized_document_is_rejected_before_memory_admission_or_decode() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("oversized");
    File::create(&path)
        .unwrap()
        .set_len(DOCUMENT_LIMIT + 1)
        .unwrap();
    let governor = governor();
    let limits = governor.usage().limits;
    let _transport = governor
        .reserve(
            Category::Transport,
            limits.categories[Category::Transport as usize],
            None,
        )
        .unwrap();
    let _margin = governor
        .reserve(
            Category::Margin,
            limits.categories[Category::Margin as usize],
            None,
        )
        .unwrap();
    assert!(matches!(
        read_json_in::<Identity>(&path, &governor),
        Err(Error::Contract("repository document type/size"))
    ));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), DOCUMENT_LIMIT + 1);
}
