use super::*;
#[test]
fn preflight_denies_before_instrumented_io() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let b = Boundary {
        path: root.into(),
        root: "/".into(),
        token: vec![1],
        class: SourceClass::Local,
    };
    for class in [
        SourceClass::Remote,
        SourceClass::Unknown,
        SourceClass::Local,
    ] {
        let mut table = vec![
            b.clone(),
            Boundary {
                path: root.join("nested"),
                root: "/".into(),
                token: vec![2],
                class,
            },
        ];
        let mut acquisitions = 0;
        if authorize(
            &table,
            root,
            &root.join("nested/file"),
            &b,
            Policy::default(),
        )
        .is_ok()
        {
            acquisitions += 1;
        }
        assert_eq!(acquisitions, 0);
        assert_eq!(
            authorize(
                &table,
                root,
                &root.join("nested/file"),
                &b,
                Policy {
                    same_volume: false,
                    explicit_root: true
                }
            )
            .is_ok(),
            class == SourceClass::Local
        );
        table[0].token = vec![3];
        assert!(authorize(&table, root, root, &b, Policy::default()).is_err());
    }
    for p in [root.join("../escape"), PathBuf::from("/elsewhere")] {
        assert!(authorize(std::slice::from_ref(&b), root, &p, &b, Policy::default()).is_err());
    }
}
#[test]
fn id_without_creation_is_uncertain_and_reuse_is_not_continuity() {
    let mut a = Stamp {
        properties: Default::default(),
        identity: Some(LocalFileId::Unix {
            device: 1,
            inode: 2,
        }),
        created: None,
        modified: None,
        length: 0,
        directory: true,
        regular: false,
    };
    assert_eq!(a.attachment(&a), Attachment::Uncertain);
    a.created = Some(SystemTime::UNIX_EPOCH);
    assert_eq!(a.attachment(&a), Attachment::Compatible);
    let mut b = a.clone();
    b.created = Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1));
    assert_eq!(a.attachment(&b), Attachment::Different);
}
#[cfg(target_os = "linux")]
#[test]
fn mount_table_keeps_automounts_and_native_names_without_probing() {
    let t=linux::parse(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 0:2 / /auto rw - autofs systemd rw\n3 1 8:1 /sub /alias\\040x\xff rw - ext4 /dev/a rw\n").unwrap();
    assert_eq!(t.len(), 3);
    assert_eq!(t[1].class, SourceClass::Unknown);
    assert_ne!(t[0].root, t[2].root);
    assert!(
        authorize(
            &t,
            Path::new("/"),
            Path::new("/auto/child"),
            &t[0],
            Policy::default()
        )
        .is_err()
    );
    assert!(linux::parse(b"broken").is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn mount_table_last_occurrence_preserves_order_tokens_and_hidden_boundaries() {
    let table = linux::parse(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 0:2 / /auto rw - autofs systemd rw\n3 2 8:1 / /auto/deep rw - ext4 /dev/a rw\n4 1 8:1 /sub /alias\\040x\xff rw - ext4 /dev/a rw\n5 1 8:1 /replaced / rw shared:1 - ext4 /dev/a ro\n").unwrap();
    assert_eq!(table.len(), 4);
    assert_eq!(table[0].path, Path::new("/auto"));
    assert_eq!(table[1].path, Path::new("/auto/deep"));
    assert_eq!(table[3].path, Path::new("/"));
    assert_eq!(table[3].root, Path::new("/replaced"));
    assert!(table[3].token.ends_with(b"shared:1 - ext4 /dev/a ro"));
    // A deeper local mount cannot hide the crossed automount from authorization.
    assert!(
        authorize(
            &table,
            Path::new("/"),
            Path::new("/auto/deep/file"),
            &table[3],
            Policy {
                same_volume: false,
                explicit_root: false
            }
        )
        .is_err()
    );
    let mut changed = table.clone();
    changed[3].token.push(b' ');
    assert!(
        authorize(
            &changed,
            Path::new("/"),
            Path::new("/file"),
            &table[3],
            Policy::default()
        )
        .is_err()
    );
    for line in [
        &b""[..],
        b"- 0 8:1 / / rw - ext4 /dev/a rw",
        b"1 0 8:1 / / - ext4 /dev/a rw",
        b"1 0 8:1 / / rw ext4 /dev/a rw",
        b"1 0 8:1 / / rw - ext4 /dev/a",
        b"1 0 8:1 / relative rw - ext4 /dev/a rw",
        b"1 0 8:1 / /bad\\000 rw - ext4 /dev/a rw",
        b"1 0 8:1 /bad\\400 / rw - ext4 /dev/a rw",
        b"1 0 8:1 / /bad\\8 rw - ext4 /dev/a rw",
    ] {
        assert!(linux::parse(line).is_err(), "accepted {line:?}");
    }
}
#[test]
fn native_files_directories_hardlinks_and_root_replacement() {
    let f = tempfile::tempdir().unwrap();
    let root = f.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file"), b"abc").unwrap();
    fs::hard_link(root.join("file"), root.join("alias")).unwrap();
    let g = Guard::open(&root, Policy::default()).unwrap();
    let a = g.inspect(&root.join("file")).unwrap();
    assert_eq!(a.length, 3);
    assert!(!a.directory);
    assert_eq!(a.identity, g.inspect(&root.join("alias")).unwrap().identity);
    assert!(g.inspect(&root).unwrap().directory);
    assert_eq!(g.read_dir(&root).unwrap().count(), 2);
    fs::rename(&root, f.path().join("old")).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(g.read_dir(&root).is_err());
}
#[cfg(unix)]
#[test]
fn no_follow_even_for_ancestor_and_broken_symlink() {
    let f = tempfile::tempdir().unwrap();
    let root = f.path().join("root");
    fs::create_dir(&root).unwrap();
    let g = Guard::open(&root, Policy::default()).unwrap();
    std::os::unix::fs::symlink("/does-not-exist", root.join("link")).unwrap();
    // Mount preflight alone preserves the legacy walker's own no-follow
    // metadata semantics; it neither probes nor follows this broken link.
    assert!(g.authorize_path(&root.join("link")).is_ok());
    assert!(g.inspect(&root.join("link")).is_err());
    fs::rename(&root, f.path().join("old")).unwrap();
    std::os::unix::fs::symlink(f.path().join("old"), &root).unwrap();
    assert!(g.read_dir(&root).is_err());
}
#[cfg(windows)]
#[test]
fn junction_and_implicit_unc_are_rejected_without_following() {
    let f = tempfile::tempdir().unwrap();
    let root = f.path().join("root");
    fs::create_dir(&root).unwrap();
    let other = f.path().join("other");
    fs::create_dir(&other).unwrap();
    let g = Guard::open(&root, Policy::default()).unwrap();
    let link = root.join("junction");
    assert!(
        std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&other)
            .output()
            .unwrap()
            .status
            .success()
    );
    let result = g.read_dir(&link);
    fs::remove_dir(link).unwrap();
    assert!(result.is_err());
    assert!(
        Guard::open(
            Path::new(r"\\not-contacted.invalid\share"),
            Policy::default()
        )
        .is_err()
    );
}

#[test]
fn store_containment_is_component_based_and_never_a_source_identity() {
    #[cfg(unix)]
    {
        assert!(contains_path(
            Path::new("/source"),
            Path::new("/source/cache")
        ));
        assert!(!contains_path(
            Path::new("/source"),
            Path::new("/source-other/cache")
        ));
    }
    #[cfg(windows)]
    {
        assert!(contains_path(
            Path::new(r"c:\Users\Example"),
            Path::new(r"\\?\C:\USERS\example\cache")
        ));
        assert!(!contains_path(
            Path::new(r"C:\Users\Example"),
            Path::new(r"C:\Users\Example-other\cache")
        ));
        assert!(local_state_location(Path::new(r"\\offline.invalid\share\cache")).is_err());
    }
}

#[test]
fn store_placement_checks_local_ancestors_before_creating_artifacts() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let g = Guard::open(&source, Policy::default()).unwrap();
    assert!(!g.store_outside(&source.join("missing/cache")).unwrap());
    assert!(g.store_outside(&t.path().join("external-cache")).unwrap());
    assert!(!source.join("missing").exists());
    let external = t.path().join("canonical-cache");
    std::fs::create_dir(&external).unwrap();
    assert!(
        g.store_outside(&std::fs::canonicalize(&external).unwrap())
            .unwrap()
    );
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn observation_properties_are_native_and_owner_collection_is_explicit() {
    use crate::{MetadataValue as M, ValueAccuracy as A};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let path = root.join("file");
    fs::write(&path, b"native metadata").unwrap();
    fs::hard_link(&path, root.join("alias")).unwrap();
    let guard = Guard::open(&root, Policy::default()).unwrap();
    assert_eq!(
        guard.inspect(&path).unwrap().properties,
        NodeProperties::default()
    );
    assert_eq!(
        guard
            .observe_options(&path, false, false)
            .unwrap()
            .properties
            .allocated,
        M::NotCollected
    );
    let a = guard.observe(&path, false).unwrap();
    assert_eq!(a.properties.owner.id, M::NotCollected);
    assert!(matches!(
        a.properties.allocated,
        M::Known {
            accuracy: A::Exact,
            ..
        }
    ));
    assert!(matches!(
        a.properties.accessed,
        M::Known {
            accuracy: A::Exact,
            ..
        }
    ));
    let b = guard.observe(&path, true).unwrap();
    let alias = guard.observe(&root.join("alias"), true).unwrap();
    assert!(matches!(
        b.properties.owner.id,
        M::Known {
            accuracy: A::Exact,
            ..
        }
    ));
    assert_eq!(b.identity, alias.identity);
    assert_eq!(b.properties.allocated, alias.properties.allocated);
    assert_eq!(b.properties.owner, alias.properties.owner);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let sparse = root.join("sparse");
        let f = fs::File::create(&sparse).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
        drop(f);
        let p = guard.observe(&sparse, true).unwrap();
        let m = fs::symlink_metadata(&sparse).unwrap();
        assert_eq!(
            p.properties.allocated,
            M::Known {
                value: m.blocks() * 512,
                accuracy: A::Exact
            }
        );
        assert_eq!(
            p.properties.owner.id,
            M::Known {
                value: crate::NativeOwnerId::UnixUid(m.uid()),
                accuracy: A::Exact
            }
        );
    }
}

#[test]
fn explicit_network_root_never_authorizes_nested_mounts_or_changed_identity() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    for class in [SourceClass::Remote, SourceClass::Unknown] {
        let original = Boundary {
            path: root.to_owned(),
            root: "/export".into(),
            token: b"server/export/access-context".to_vec(),
            class,
        };
        let child = root.join("file");
        let mut table = vec![original.clone()];
        assert!(authorize(&table, root, &child, &original, Policy::default()).is_err());
        let explicit = Policy {
            explicit_root: true,
            ..Policy::default()
        };
        assert!(authorize(&table, root, &child, &original, explicit).is_ok());
        for nested_class in [
            SourceClass::Local,
            SourceClass::Remote,
            SourceClass::Unknown,
        ] {
            table.push(Boundary {
                path: root.join("nested"),
                root: "/different".into(),
                token: b"different-source".to_vec(),
                class: nested_class,
            });
            assert!(
                authorize(&table, root, &root.join("nested/file"), &original, explicit).is_err()
            );
            table.pop();
        }
        table[0].token = b"other-server/export/access-context".to_vec();
        assert!(authorize(&table, root, &child, &original, explicit).is_err());
        // A fresh explicit guard may authorize the new source, but an earlier
        // authorization is never silently transferred across that change.
        assert!(authorize(&table, root, &child, &table[0], explicit).is_ok());
    }
}
