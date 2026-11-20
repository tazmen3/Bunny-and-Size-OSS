use super::*;
use crate::{
    inventory::tests::{attempt_id, batch, context_id, fold, observation, reconcile},
    scheduler::{Session, State},
};
fn root() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/never-acquire-this-offline-root")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\offline.invalid\share\never-acquire-this-root")
    }
}
#[test]
fn offline_open_does_not_create_history_and_handoff_is_durable_idempotent() {
    let t = tempfile::tempdir().unwrap();
    let mut s = Store::create(
        &t.path().join("source-cache"),
        InventoryId(u128::MAX),
        SourceId(9),
        Durability::Retained,
    )
    .unwrap();
    s.create_generation(context_id(0)).unwrap();
    let a = attempt_id(0, 0, 1);
    s.begin_attempt(a).unwrap();
    batch(
        &mut s,
        a,
        0,
        Some(ListingEnd::Exhausted),
        &[observation(240, Kind::File)],
    )
    .unwrap();
    reconcile(&mut s, a);
    fold(&mut s, 0);
    s.seal(context_id(0)).unwrap();
    s.record_measurement(GenerationId(0), &root(), 100, 200)
        .unwrap();
    let consumer = t.path().join("history");
    lifecycle::ensure_root(&consumer).unwrap();
    let lease = s.lease(GenerationId(0)).unwrap();
    assert_eq!(std::fs::read_dir(&consumer).unwrap().count(), 0);
    assert_eq!(lease.measurement.logical, Some(240));
    let first = handoff(&lease, &consumer).unwrap();
    assert!(!first.replay);
    let second = handoff(&lease, &consumer).unwrap();
    assert!(second.replay);
    assert_eq!(first.directory, second.directory);
    drop(lease);
    s.forget().unwrap();
    let durable = Store::reopen(&first.directory, Durability::Retained).unwrap();
    assert_eq!(
        durable.measurement(GenerationId(0)).unwrap().logical,
        Some(240)
    );
    assert!(durable.forget().is_err());
    assert!(Store::reopen(&first.directory, Durability::Retained).is_ok());
    assert_eq!(
        std::fs::read_dir(&consumer)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("inv-")))
            .count(),
        1
    );
}
#[test]
fn retained_candidate_resumes_explicitly_after_process_owner_reopens() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("actual-source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("f"), b"1234567").unwrap();
    let guard = sortsize_core::acquisition::Guard::open(&source, Default::default()).unwrap();
    let c = context_id(0);
    let store = Store::create(
        &t.path().join("temporary"),
        c.inventory,
        c.source,
        Durability::Reconstructible,
    )
    .unwrap();
    let mut session = Session::new(store, c, guard).unwrap();
    session
        .store_mut()
        .record_acquisition(c, &source, 100)
        .unwrap();
    let state = t.path().join("state");
    lifecycle::ensure_root(&state).unwrap();
    let destination = state.join("inv-fixture");
    let copy = session.store_mut().retain_copy(&destination, 30).unwrap();
    session.replace_store(copy).unwrap();
    drop(session);
    let store = Store::reopen(&destination, Durability::Retained).unwrap();
    let saved = store.saved(GenerationId(0)).unwrap();
    assert_eq!(saved.measured, None);
    assert!(store.measurements().unwrap().is_empty());
    let guard = sortsize_core::acquisition::Guard::open(&source, Default::default()).unwrap();
    let mut session = Session::resume(store, GenerationId(0), guard).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while session.tick().unwrap().state != State::Idle {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    while session.finalize_step().unwrap().is_none() {}
    session
        .store_mut()
        .record_measurement(GenerationId(0), &source, 100, 200)
        .unwrap();
    assert_eq!(
        session.store().saved(GenerationId(0)).unwrap().logical,
        Some(7)
    );
    drop(session);
    let c = list(&state, None).unwrap();
    assert_eq!(c.rows.len(), 1);
    assert_eq!(c.rows[0].measured, Some(200));
}
#[cfg(windows)]
#[test]
fn private_windows_acl_is_protected_owner_and_system_only() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW,
            DACL_SECURITY_INFORMATION, GetFileSecurityW,
        },
    };
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("private");
    lifecycle::ensure_root(&path).unwrap();
    std::fs::write(path.join("sentinel"), b"private metadata").unwrap();
    assert_eq!(
        std::fs::read(path.join("sentinel")).unwrap(),
        b"private metadata"
    );
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut needed = 0;
    unsafe {
        GetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    assert!(needed > 0 && needed < 64 * 1024);
    let mut sd = vec![0u8; needed as usize];
    unsafe {
        assert_ne!(
            GetFileSecurityW(
                name.as_ptr(),
                DACL_SECURITY_INFORMATION,
                sd.as_mut_ptr().cast(),
                needed,
                &mut needed
            ),
            0
        );
        let mut out = std::ptr::null_mut();
        assert_ne!(
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd.as_mut_ptr().cast(),
                1,
                DACL_SECURITY_INFORMATION,
                &mut out,
                std::ptr::null_mut()
            ),
            0
        );
        let mut len = 0;
        while *out.add(len) != 0 {
            len += 1;
            assert!(len < 32768);
        }
        let sddl = String::from_utf16(std::slice::from_raw_parts(out, len)).unwrap();
        LocalFree(out.cast());
        assert!(sddl.starts_with("D:P"), "{sddl}");
        assert!(sddl.contains(";;;SY)") && sddl.contains(";;;OW)"), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 2, "{sddl}");
        eprintln!("protected owner/SYSTEM DACL: {sddl}");
    }
}

#[cfg(windows)]
#[test]
fn existing_unprotected_windows_parent_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("unprotected");
    std::fs::create_dir(&path).unwrap();
    assert!(lifecycle::ensure_root(&path).is_err());
}
