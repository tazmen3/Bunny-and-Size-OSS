//! Explicit real-system tests. Ordinary cargo test never invokes host identity
//! providers: opt in with --test owner_resolution_native -- --ignored.
use sortsize_core::*;
use sortsize_owner_resolution::*;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn resolve(provider: Arc<SystemOwnerLookup>, id: NativeOwnerId) -> OwnerNameResolution {
    let r = OwnerResolver::new(ResolverConfig::default(), provider).unwrap();
    assert_eq!(r.try_submit(id), SubmitResult::Queued);
    let start = Instant::now();
    loop {
        if let Some(value) = r.try_take() {
            assert!(value.id == id, "identity mismatch (redacted)");
            return value.resolution;
        }
        // Test harness deadline only; this does not interrupt an OS API.
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "provider still blocked; not a cancellable timeout"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn provider() -> Arc<SystemOwnerLookup> {
    Arc::new(SystemOwnerLookup::new(
        SystemProviderConsent::AllowPotentialNetworkLookup,
    ))
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "Explicit opt-in: NSS may contact remote identity servers"]
fn linux_current_account_matches_system_oracle_and_missing_uid() {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::TempDir::new().unwrap();
    let uid = std::fs::metadata(temp.path()).unwrap().uid();
    let p = provider();
    let result = resolve(p.clone(), NativeOwnerId::UnixUid(uid));
    let oracle = std::process::Command::new("id")
        .args(["-un", &uid.to_string()])
        .output()
        .unwrap();
    assert!(oracle.status.success());
    let expected = oracle.stdout.strip_suffix(b"\n").unwrap();
    assert!(
        result == OwnerNameResolution::Resolved(AccountName::UnixBytes(expected.into())),
        "native account mismatch (redacted)"
    );
    assert_eq!(
        resolve(p.clone(), NativeOwnerId::UnixUid(u32::MAX)),
        OwnerNameResolution::NotFound
    );
    assert_eq!(p.native_calls(), 2);
    println!(
        "native Linux current account and unmapped UID passed; getpwuid_r calls=2; no identity logged"
    );
}
#[cfg(windows)]
#[test]
#[ignore = "Explicit opt-in: Windows may contact domain controllers"]
fn windows_current_account_and_unmapped_sid_after_source_removal() {
    let temp = tempfile::TempDir::new().unwrap();
    let report = scan_inventory_with_owners(
        temp.path(),
        &ScanOptions::default(),
        &CancellationToken::default(),
        InventoryLimits::default(),
        OwnerSelection {
            owner: true,
            metadata: MetadataSelection {
                dates: false,
                allocation: false,
            },
        },
        |_| {},
    )
    .unwrap();
    let MetadataValue::Known { value: id, .. } = report.owners[0].id else {
        panic!("expected native SID");
    };
    drop(temp);
    let p = provider();
    let OwnerNameResolution::Resolved(name) = resolve(p.clone(), id) else {
        panic!("expected resolved current account");
    };
    assert!(!name.to_string_lossy().is_empty());
    let unmapped = WindowsSid::from_bytes(&[1, 1, 0, 0, 0, 0, 0, 5, 255, 255, 255, 255]).unwrap();
    assert_eq!(
        resolve(p.clone(), NativeOwnerId::WindowsSid(unmapped)),
        OwnerNameResolution::NotFound
    );
    assert_eq!(p.native_calls(), 3);
    println!(
        "native Windows current account and unmapped SID passed; LookupAccountSidW calls=3; no identity logged"
    );
}

#[cfg(windows)]
#[test]
#[ignore = "Explicit opt-in: Windows may contact domain controllers"]
fn windows_token_user_matches_independent_sam_name_oracle() {
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };
    use windows_sys::Win32::{
        Security::{
            Authentication::Identity::{GetUserNameExW, NameSamCompatible},
            GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = ptr::null_mut();
    // SAFETY: valid pseudo-handle, query-only access and initialized output.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    // SAFETY: uniquely owned handle returned by successful OpenProcessToken.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0;
    // SAFETY: documented null-buffer sizing query on a live handle.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    assert!((size_of::<TOKEN_USER>()..=1024 * 1024).contains(&(needed as usize)));
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: DWORD/pointer-aligned buffer covering the queried size.
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        },
        0
    );
    // SAFETY: successful TokenUser initialized TOKEN_USER and a valid SID inside
    // its live buffer. Copy its checked length before releasing that buffer.
    let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let length = unsafe { GetLengthSid(sid) } as usize;
    assert!((8..=68).contains(&length));
    let sid =
        WindowsSid::from_bytes(unsafe { std::slice::from_raw_parts(sid.cast(), length) }).unwrap();
    let OwnerNameResolution::Resolved(name) = resolve(provider(), NativeOwnerId::WindowsSid(sid))
    else {
        panic!("expected current token user name");
    };
    let mut oracle = vec![0u16; 32768];
    let mut count = oracle.len() as u32;
    // SAFETY: writable UTF-16 buffer with exact supplied capacity and size output.
    assert!(unsafe { GetUserNameExW(NameSamCompatible, oracle.as_mut_ptr(), &mut count) });
    assert!(
        name.to_string_lossy().to_lowercase()
            == String::from_utf16_lossy(&oracle[..count as usize]).to_lowercase(),
        "native account mismatch (redacted)"
    );
    println!("native Windows token user matches independent SAM name oracle; no identity logged");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "Explicit opt-in: NSS may contact remote identity servers"]
fn linux_tiny_native_buffer_is_unavailable_without_truncation() {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::TempDir::new().unwrap();
    let uid = std::fs::metadata(temp.path()).unwrap().uid();
    let p = provider();
    assert_eq!(
        p.lookup(
            NativeOwnerId::UnixUid(uid),
            LookupLimits {
                max_buffer_bytes: 1,
                max_name_bytes: 64
            }
        ),
        OwnerNameResolution::Unavailable(LookupError::BufferLimit)
    );
    assert_eq!(p.native_calls(), 1);
}

#[cfg(windows)]
#[test]
#[ignore = "Explicit opt-in: invokes the Windows identity provider"]
fn windows_native_buffer_limits_and_exact_utf16_match_win32_oracle() {
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError},
        Security::LookupAccountSidW,
    };
    // Public well-known LocalSystem SID, DWORD aligned; no personal identity.
    let mut sid_words = [0x0000_0101u32, 0x0500_0000, 18];
    let sid_ptr = sid_words.as_mut_ptr().cast();
    let (mut names, mut domains, mut kind) = (0u32, 0u32, 0i32);
    // SAFETY: complete aligned SID, documented null-buffer sizing query.
    let ok = unsafe {
        LookupAccountSidW(
            ptr::null(),
            sid_ptr,
            ptr::null_mut(),
            &mut names,
            ptr::null_mut(),
            &mut domains,
            &mut kind,
        )
    };
    assert_eq!(ok, 0);
    assert_eq!(unsafe { GetLastError() }, ERROR_INSUFFICIENT_BUFFER);
    assert!(names > 1 && names < 32768 && domains < 32768);
    let mut name = vec![0u16; names as usize];
    let mut domain = vec![0u16; domains as usize];
    let (mut tiny_names, mut tiny_domains) = (1, 1);
    // SAFETY: one writable unit in each buffer; API must reject short buffers.
    let ok = unsafe {
        LookupAccountSidW(
            ptr::null(),
            sid_ptr,
            name.as_mut_ptr(),
            &mut tiny_names,
            domain.as_mut_ptr(),
            &mut tiny_domains,
            &mut kind,
        )
    };
    assert_eq!(ok, 0);
    assert_eq!(unsafe { GetLastError() }, ERROR_INSUFFICIENT_BUFFER);
    // SAFETY: Rust-owned output buffers have the exact queried capacities.
    assert_ne!(
        unsafe {
            LookupAccountSidW(
                ptr::null(),
                sid_ptr,
                name.as_mut_ptr(),
                &mut names,
                domain.as_mut_ptr(),
                &mut domains,
                &mut kind,
            )
        },
        0
    );
    name.truncate(names as usize);
    domain.truncate(domains as usize);
    if !domain.is_empty() {
        domain.push(b'\\'.into());
    }
    domain.extend_from_slice(&name);
    let sid = WindowsSid::from_bytes(&[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]).unwrap();
    let id = NativeOwnerId::WindowsSid(sid);
    let p = provider();
    assert!(
        p.lookup(id, ResolverConfig::default().lookup_limits)
            == OwnerNameResolution::Resolved(AccountName::WindowsUtf16(domain.into())),
        "native UTF-16 mismatch (redacted)"
    );
    assert_eq!(p.native_calls(), 2);
    for limits in [
        LookupLimits {
            max_buffer_bytes: 1,
            max_name_bytes: 32768,
        },
        LookupLimits {
            max_buffer_bytes: 65536,
            max_name_bytes: 1,
        },
    ] {
        assert_eq!(
            p.lookup(id, limits),
            OwnerNameResolution::Unavailable(LookupError::BufferLimit)
        );
    }
    assert_eq!(p.native_calls(), 5); // sizing only, then sizing + fill.
    assert_eq!(owner_id_text(id).as_deref(), Some("S-1-5-18"));
    println!(
        "native sizing/short buffers=ERROR_INSUFFICIENT_BUFFER; exact qualified UTF-16 and bounded provider passed; provider API calls=5, oracle calls=3; no identity logged"
    );
}
