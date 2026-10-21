#![cfg(windows)]
use sortsize_core::*;
use std::{
    fs,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetLengthSid, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
    },
};

// Independent API and caller-owned aligned buffer, no production owner helper.
fn oracle(path: &Path) -> WindowsSid {
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut needed = 0;
    // SAFETY: terminated native path, size query with no output buffer.
    unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            OWNER_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    assert!(needed > 0);
    let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
    // SAFETY: aligned buffer has at least needed bytes and valid size output.
    assert_ne!(
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                OWNER_SECURITY_INFORMATION,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        },
        0
    );
    let mut sid = ptr::null_mut();
    let mut defaulted = 0;
    // SAFETY: successful GetFileSecurityW initialized this live descriptor.
    assert_ne!(
        unsafe {
            GetSecurityDescriptorOwner(buffer.as_ptr().cast_mut().cast(), &mut sid, &mut defaulted)
        },
        0
    );
    assert!(!sid.is_null());
    // SAFETY: SID is owned by the still-live valid descriptor.
    let length = unsafe { GetLengthSid(sid) } as usize;
    assert!((8..=68).contains(&length));
    // SAFETY: copy exactly the length returned for this valid OS SID.
    WindowsSid::from_bytes(unsafe { std::slice::from_raw_parts(sid.cast(), length) }).unwrap()
}

fn scan(root: &Path, workers: usize, selection: OwnerSelection) -> InventoryWithOwners {
    let mut options = ScanOptions::default();
    options.concurrency.workers = workers;
    scan_inventory_with_owners(
        root,
        &options,
        &CancellationToken::default(),
        InventoryLimits::default(),
        selection,
        |_| {},
    )
    .unwrap()
}

#[test]
fn binary_sid_validation_and_limit_preserve_exact_bytes() {
    for count in [0, 1, 5, 15] {
        let mut bytes = vec![0; 8 + 4 * count];
        bytes[0] = 1;
        bytes[1] = count as u8;
        bytes[7] = 5;
        let sid = WindowsSid::from_bytes(&bytes).unwrap();
        assert_eq!(sid.as_bytes(), bytes);
        bytes.push(0);
        assert!(WindowsSid::from_bytes(&bytes).is_none());
    }
    for bytes in [
        vec![],
        vec![0; 8],
        vec![1, 16, 0, 0, 0, 0, 0, 5],
        vec![1, 1, 0, 0, 0, 0, 0, 5],
    ] {
        assert!(WindowsSid::from_bytes(&bytes).is_none());
    }
    let mut first = [0; 12];
    first[0] = 1;
    first[1] = 1;
    let mut second = first;
    second[8] = 1;
    assert_ne!(
        WindowsSid::from_bytes(&first),
        WindowsSid::from_bytes(&second)
    );
}

#[test]
fn fresh_fixture_owner_matches_the_current_account_token() {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Security::{
            GetTokenInformation, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = ptr::null_mut();
    // SAFETY: process pseudo-handle, query-only rights and valid output pointer.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    // SAFETY: successful OpenProcessToken returned a uniquely owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0;
    // SAFETY: valid token handle, null buffer for required-length query.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    assert!(needed as usize >= std::mem::size_of::<TOKEN_USER>());
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: aligned live buffer with sufficient length, valid token and size.
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
    // SAFETY: successful TokenUser query initialized TOKEN_USER and its SID.
    let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    // SAFETY: valid SID remains in the live token-information buffer.
    let length = unsafe { GetLengthSid(sid) } as usize;
    assert!((8..=68).contains(&length));
    // SAFETY: exactly the valid SID's reported byte length, copied before free.
    let expected =
        WindowsSid::from_bytes(unsafe { std::slice::from_raw_parts(sid.cast(), length) }).unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let actual = oracle(temp.path());
    eprintln!("fixture_owner_matches_token_user={}", actual == expected);
    if actual != expected {
        // Elevated tokens may default new objects to a group. Query that
        // documented default without assuming administrator/non-admin state.
        let mut needed = 0;
        // SAFETY: live token and valid size output for the query-only call.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenOwner,
                ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        assert!(needed as usize >= std::mem::size_of::<TOKEN_OWNER>());
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: aligned buffer covers needed bytes and token is live.
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    TokenOwner,
                    buffer.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            },
            0
        );
        // SAFETY: successful TokenOwner query initialized the complete structure.
        let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_OWNER>())).Owner };
        // SAFETY: valid OS SID in the live buffer.
        let length = unsafe { GetLengthSid(sid) } as usize;
        assert!((8..=68).contains(&length));
        // SAFETY: copy only the reported SID bytes before buffer destruction.
        let default_owner =
            WindowsSid::from_bytes(unsafe { std::slice::from_raw_parts(sid.cast(), length) })
                .unwrap();
        assert!(
            actual == default_owner,
            "fixture owner differs from token default"
        );
    }
}

#[test]
fn all_selections_workers_and_native_oracle_include_root_hardlinks_sparse_long_unicode() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::create_dir(root.join("répertoire-東京")).unwrap();
    let file = root.join("répertoire-東京/fichier");
    fs::write(&file, b"1234567").unwrap();
    fs::hard_link(&file, root.join("hardlink")).unwrap();
    fs::write(root.join("empty"), []).unwrap();
    let sparse = root.join("sparse");
    fs::File::create(&sparse).unwrap().set_len(1 << 24).unwrap();
    let status = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(&sparse)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "temporary NTFS sparse fixture must be available"
    );
    let mut deep = root.clone();
    for _ in 0..8 {
        deep = deep.join("long-abcdefghijklmnopqrstuvwxyz-été");
        fs::create_dir(&deep).unwrap();
    }
    fs::write(deep.join("終わり"), []).unwrap();
    let expected = oracle(&root);
    for workers in [1, 4, 8] {
        for flags in 0..8 {
            let selection = OwnerSelection {
                owner: flags & 1 != 0,
                metadata: MetadataSelection {
                    dates: flags & 2 != 0,
                    allocation: flags & 4 != 0,
                },
            };
            let r = scan(&root, workers, selection);
            assert_eq!(r.metadata.inventory.report.coverage, Coverage::Complete);
            assert_eq!(r.metadata.inventory.report.files, 5);
            assert_eq!(
                r.owners.len(),
                if selection.owner {
                    r.metadata.inventory.entries.len()
                } else {
                    0
                }
            );
            for (i, entry) in r.metadata.inventory.entries.iter().enumerate() {
                if selection.owner {
                    assert_eq!(
                        r.owners[i].id,
                        MetadataValue::Known {
                            value: NativeOwnerId::WindowsSid(oracle(&entry.path)),
                            accuracy: ValueAccuracy::Exact
                        }
                    );
                    assert_eq!(
                        r.owners[i].id,
                        MetadataValue::Known {
                            value: NativeOwnerId::WindowsSid(expected),
                            accuracy: ValueAccuracy::Exact
                        }
                    );
                }
                if selection.metadata.allocation && entry.kind == InventoryKind::File {
                    assert!(matches!(
                        r.metadata.allocations[i].allocated_bytes,
                        MetadataValue::Known { .. }
                    ));
                }
            }
        }
    }
}

struct RestoreAcl {
    file: fs::File,
    descriptor: *mut std::ffi::c_void,
    dacl: *mut windows_sys::Win32::Security::ACL,
}
impl RestoreAcl {
    fn new(path: &Path) -> Self {
        let file = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap();
        let mut descriptor = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        // SAFETY: valid owned handle and output pointers; buffer retained by guard.
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        Self {
            file,
            descriptor,
            dacl,
        }
    }
}
impl Drop for RestoreAcl {
    fn drop(&mut self) {
        // SAFETY: pre-opened WRITE_DAC handle and DACL in live descriptor. No
        // path re-open is needed while READ_CONTROL is denied on the fixture.
        let status = unsafe {
            SetSecurityInfo(
                self.file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                self.dacl,
                ptr::null_mut(),
            )
        };
        // SAFETY: unique OS allocation from GetSecurityInfo, no subsequent use.
        unsafe {
            LocalFree(self.descriptor);
        }
        assert_eq!(status, 0, "fixture ACL restoration failed");
    }
}

#[test]
fn real_read_control_denial_keeps_allocation_dates_and_complete_coverage() {
    let temp = tempfile::TempDir::new().unwrap();
    let file = temp.path().join("denied-owner");
    fs::write(&file, b"1234567").unwrap();
    let guard = RestoreAcl::new(&file);
    // OWNER RIGHTS overrides the owner's implicit READ_CONTROL. Only our fresh
    // fixture is touched; saved DACL is restored using a pre-opened handle.
    let output = std::process::Command::new("icacls")
        .arg(&file)
        .args(["/deny", "*S-1-3-4:(RC)", "/Q"])
        .output()
        .unwrap();
    assert!(output.status.success());
    for workers in [1, 4, 8] {
        for dates in [false, true] {
            for allocation in [false, true] {
                #[cfg(feature = "owner-profiling")]
                let before = sortsize_core::owner_profile::snapshot();
                let r = scan(
                    temp.path(),
                    workers,
                    OwnerSelection {
                        owner: true,
                        metadata: MetadataSelection { dates, allocation },
                    },
                );
                assert_eq!(r.metadata.inventory.report.coverage, Coverage::Complete);
                assert_eq!(r.metadata.inventory.report.logical_bytes, 7);
                let i = r
                    .metadata
                    .inventory
                    .entries
                    .iter()
                    .position(|e| e.kind == InventoryKind::File)
                    .unwrap();
                assert_eq!(r.owners[i].id, MetadataValue::AccessDenied);
                if allocation {
                    assert!(matches!(
                        r.metadata.allocations[i].allocated_bytes,
                        MetadataValue::Known { .. }
                    ));
                }
                if dates {
                    assert!(matches!(
                        r.metadata.dates[i].modified,
                        MetadataValue::Known { .. }
                    ));
                }
                #[cfg(feature = "owner-profiling")]
                {
                    let after = sortsize_core::owner_profile::snapshot();
                    // Serial profiling recipe: root succeeds, file combined
                    // open is denied, fallback succeeds, no file security call.
                    assert_eq!(
                        [
                            after[0] - before[0],
                            after[1] - before[1],
                            after[2] - before[2]
                        ],
                        [3, 2, 1]
                    );
                    assert_eq!(after[4], 0);
                }
            }
        }
    }
    drop(guard);
    let _ = oracle(&file);
}

#[test]
fn junction_and_available_symlink_are_not_followed() {
    let temp = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    fs::write(outside.path().join("hidden"), []).unwrap();
    let junction = temp.path().join("junction");
    let output = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let symlink = temp.path().join("symlink");
    let symlink_available = std::os::windows::fs::symlink_dir(outside.path(), &symlink).is_ok();
    eprintln!("symlink_available={symlink_available}");
    for workers in [1, 4, 8] {
        let r = scan(
            temp.path(),
            workers,
            OwnerSelection {
                owner: true,
                ..Default::default()
            },
        );
        assert_eq!(r.metadata.inventory.report.files, 0);
        for (entry, owner) in r.metadata.inventory.entries.iter().zip(&r.owners) {
            if entry.kind == InventoryKind::Symlink {
                assert_eq!(owner.id, MetadataValue::NotCollected);
            }
        }
    }
    fs::remove_dir(junction).unwrap();
    if symlink_available {
        fs::remove_dir(symlink).unwrap();
    }
    assert!(outside.path().join("hidden").exists());
}

#[test]
fn native_denied_directory_listing_keeps_owner_and_restores_access() {
    let temp = tempfile::TempDir::new().unwrap();
    let blocked = temp.path().join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("hidden"), []).unwrap();
    let guard = RestoreAcl::new(&blocked);
    let output = std::process::Command::new("icacls")
        .arg(&blocked)
        .args(["/deny", "*S-1-1-0:(RD)", "/Q"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(fs::read_dir(&blocked).is_err());
    for workers in [1, 4, 8] {
        let r = scan(
            temp.path(),
            workers,
            OwnerSelection {
                owner: true,
                ..Default::default()
            },
        );
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Partial);
        assert_eq!(r.metadata.inventory.report.files, 0);
        assert_eq!(r.owners.len(), 2);
        assert!(r.owners.iter().all(|o| matches!(
            o.id,
            MetadataValue::Known {
                accuracy: ValueAccuracy::Exact,
                ..
            }
        )));
    }
    drop(guard);
    assert_eq!(fs::read_dir(blocked).unwrap().count(), 1);
}

#[test]
fn root_directory_owner_denial_does_not_stop_listing() {
    let temp = tempfile::TempDir::new().unwrap();
    fs::write(temp.path().join("file"), b"xyz").unwrap();
    let guard = RestoreAcl::new(temp.path());
    let output = std::process::Command::new("icacls")
        .arg(temp.path())
        .args(["/deny", "*S-1-3-4:(RC)", "/Q"])
        .output()
        .unwrap();
    assert!(output.status.success());
    for workers in [1, 4, 8] {
        let r = scan(
            temp.path(),
            workers,
            OwnerSelection {
                owner: true,
                ..Default::default()
            },
        );
        assert_eq!(r.metadata.inventory.report.coverage, Coverage::Complete);
        assert_eq!(r.metadata.inventory.report.files, 1);
        assert_eq!(r.owners[0].id, MetadataValue::AccessDenied);
    }
    drop(guard);
    let _ = oracle(temp.path());
}

#[test]
fn repeated_scans_release_handles_and_preserve_sid_after_descriptor_free() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    fn handles() -> u32 {
        let mut count = 0;
        // SAFETY: pseudo-handle for this process and valid writable count.
        assert_ne!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
            0
        );
        count
    }
    let temp = tempfile::TempDir::new().unwrap();
    fs::write(temp.path().join("file"), []).unwrap();
    let selection = OwnerSelection {
        owner: true,
        metadata: MetadataSelection {
            dates: true,
            allocation: true,
        },
    };
    // Run this test binary with --test-threads=1 for the process-wide leak check.
    let _ = scan(temp.path(), 8, selection);
    let before = handles();
    let mut last = None;
    for _ in 0..100 {
        last = Some(scan(temp.path(), 8, selection));
    }
    let after = handles();
    eprintln!("owner_repeated_scan_handles_before={before},after={after}");
    // Other concurrently running tests may own handles; serial validation also
    // checks equality explicitly from this reported pair.
    assert!(after <= before + 32);
    let r = last.unwrap();
    let expected = oracle(temp.path());
    assert_eq!(
        r.owners[0].id,
        MetadataValue::Known {
            value: NativeOwnerId::WindowsSid(expected),
            accuracy: ValueAccuracy::Exact
        }
    );
}
