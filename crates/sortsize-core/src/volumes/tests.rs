use super::*;
use std::cell::RefCell;
fn volume(path: &str, kind: SourceKind) -> Volume {
    Volume {
        identity: VolumeIdentity::WindowsPath(path.into()),
        label: MetadataValue::Unknown,
        filesystem: MetadataValue::Unknown,
        kind,
        mounts: vec![VolumeMount {
            path: path.into(),
            mount_id: None,
            root: None,
        }],
        capacity: VolumeCapacity::absent(MetadataValue::NotCollected),
        availability: VolumeAvailability::NotProbed,
    }
}
struct Provider {
    volumes: Vec<Volume>,
    calls: RefCell<Vec<PathBuf>>,
}
impl VolumeProvider for Provider {
    fn enumerate(&self) -> io::Result<Vec<Volume>> {
        Ok(self.volumes.clone())
    }
    fn probe(&self, path: &Path) -> VolumeProbe {
        self.calls.borrow_mut().push(path.to_owned());
        if path == Path::new("denied") {
            return VolumeProbe::error(&io::ErrorKind::PermissionDenied.into());
        }
        if path == Path::new("gone") {
            return VolumeProbe::error(&io::ErrorKind::NotFound.into());
        }
        VolumeProbe {
            capacity: VolumeCapacity::from_bytes(known(100), known(20), known(10)),
            availability: VolumeAvailability::Available,
            label: MetadataValue::Unknown,
            filesystem: MetadataValue::Unknown,
        }
    }
}
#[test]
fn injected_empty_single_aliases_partial_errors_and_network_consent() {
    let mut p = Provider {
        volumes: vec![],
        calls: RefCell::default(),
    };
    assert!(
        discover_with(&p, DiscoveryOptions::default())
            .unwrap()
            .volumes
            .is_empty()
    );
    let mut v = volume("local space", SourceKind::Local);
    v.mounts.push(VolumeMount {
        path: "alias".into(),
        mount_id: None,
        root: None,
    });
    p.volumes.push(v);
    let r = discover_with(&p, DiscoveryOptions::default()).unwrap();
    assert_eq!(r.capacity_calls, 1);
    assert_eq!(r.volumes[0].mounts.len(), 2);
    p.volumes.extend([
        volume("denied", SourceKind::Local),
        volume("gone", SourceKind::Removable),
        volume("network", SourceKind::Remote),
        volume("unknown", SourceKind::Unknown),
    ]);
    let r = discover_with(&p, DiscoveryOptions::default()).unwrap();
    assert_eq!(r.volumes.len(), 5);
    assert_eq!(r.capacity_calls, 3);
    assert_eq!(r.volumes[0].availability, VolumeAvailability::Available);
    assert_eq!(r.volumes[1].availability, VolumeAvailability::AccessDenied);
    assert_eq!(r.volumes[2].availability, VolumeAvailability::Unavailable);
    assert_eq!(r.volumes[3].capacity.total, MetadataValue::NotCollected);
    assert!(
        !p.calls
            .borrow()
            .iter()
            .any(|p| p == Path::new("network") || p == Path::new("unknown"))
    );
    let r = discover_with(
        &p,
        DiscoveryOptions {
            probe_remote: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.capacity_calls, 5);
}
#[test]
fn capacities_zero_full_unknown_denied_overflow_and_reserved_space() {
    let c = VolumeCapacity::from_bytes(known(100), known(20), known(10));
    assert_eq!(c.used, known(80));
    assert_eq!(c.utilization_percent(), known(80.0));
    let zero = VolumeCapacity::from_bytes(known(0), known(0), known(0));
    assert_eq!(zero.used, known(0));
    assert_eq!(zero.utilization_percent(), MetadataValue::Unknown);
    assert_eq!(
        VolumeCapacity::from_bytes(known(100), known(0), known(0)).utilization_percent(),
        known(100.0)
    );
    assert_eq!(
        VolumeCapacity::from_bytes(known(u64::MAX), known(1), known(0)).used,
        known(u64::MAX - 1)
    );
    for state in [
        MetadataValue::Unknown,
        MetadataValue::Unsupported,
        MetadataValue::AccessDenied,
        MetadataValue::NotCollected,
    ] {
        let c = VolumeCapacity::from_bytes(state, state, state);
        assert_eq!(c.total, state);
        assert_eq!(c.utilization_percent(), MetadataValue::Unknown);
    }
    assert_eq!(
        VolumeCapacity::from_bytes(known(1), known(2), known(0)).used,
        MetadataValue::Unknown
    );
    assert_eq!(
        VolumeCapacity::from_bytes(known(10), known(2), known(3)).available,
        MetadataValue::Unknown
    );
    assert_eq!(
        VolumeCapacity::from_bytes(MetadataValue::Unknown, known(2), known(1)).used,
        MetadataValue::Unknown
    );
}
#[test]
fn enumeration_error_is_not_an_empty_success() {
    struct Failed;
    impl VolumeProvider for Failed {
        fn enumerate(&self) -> io::Result<Vec<Volume>> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
        fn probe(&self, _: &Path) -> VolumeProbe {
            panic!("no query after enumeration failure")
        }
    }
    assert_eq!(
        discover_with(&Failed, DiscoveryOptions::default())
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn inaccessible_preferred_mount_falls_back_to_a_reachable_native_alias() {
    let mut v = volume("denied", SourceKind::Local);
    v.mounts.push(VolumeMount {
        path: "reachable alias".into(),
        mount_id: None,
        root: None,
    });
    let p = Provider {
        volumes: vec![v],
        calls: RefCell::default(),
    };
    let r = discover_with(&p, DiscoveryOptions::default()).unwrap();
    assert_eq!(r.capacity_calls, 2);
    assert_eq!(r.volumes[0].availability, VolumeAvailability::Available);
    assert_eq!(r.volumes[0].mounts[0].path, Path::new("reachable alias"));
    assert_eq!(r.volumes[0].mounts.len(), 2);
}
