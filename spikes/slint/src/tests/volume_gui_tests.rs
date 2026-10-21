use super::*;
use sortsize_core::{MetadataValue, volumes::*};
pub(super) fn discovered(paths: &[std::path::PathBuf]) -> VolumeDiscovery {
    VolumeDiscovery {
        volumes: paths
            .iter()
            .enumerate()
            .map(|(i, path)| Volume {
                identity: VolumeIdentity::WindowsPath(path.clone()),
                label: MetadataValue::Unknown,
                filesystem: MetadataValue::Unknown,
                kind: SourceKind::Local,
                mounts: vec![VolumeMount {
                    path: path.clone(),
                    mount_id: None,
                    root: None,
                }],
                capacity: VolumeCapacity::from_bytes(
                    MetadataValue::Unknown,
                    MetadataValue::Unknown,
                    MetadataValue::Unknown,
                ),
                availability: if i == 1 {
                    VolumeAvailability::Unavailable
                } else {
                    VolumeAvailability::Available
                },
            })
            .collect(),
        enumeration_time: Duration::ZERO,
        capacity_time: Duration::ZERO,
        capacity_calls: 0,
    }
}
#[test]
fn volume_selection_preserves_inventory_native_path_and_scan_requires_explicit_action() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = SortSizeWindow::new().unwrap();
    let controller = connect(&window);
    let fixture = TempDir::new().unwrap();
    fs::write(fixture.path().join("file"), b"read only").unwrap();
    window.set_scan_path(fixture.path().to_str().unwrap().into());
    window.invoke_start_scan();
    wait(&window, &controller);
    let original = window.get_current_path();
    let details = window.get_details().row_data(0).unwrap();
    #[cfg(unix)]
    let native = {
        use std::os::unix::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/volume with spaces-\xff".to_vec(),
        ))
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::ffi::OsStringExt;
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&[67, 58, 92, 0xd800]))
    };
    controller.borrow_mut().sources.current =
        Some(crate::volume_view::Prepared::new(discovered(&[
            native.clone(),
            "gone".into(),
            fixture.path().to_owned(),
        ])));
    controller.borrow().show_sources(&window);
    window.invoke_select_volume(0);
    assert_eq!(
        controller
            .borrow()
            .path_input
            .resolve(window.get_scan_path().as_str()),
        native
    );
    assert_eq!(window.get_selected_volume(), 0);
    assert!(!window.get_busy());
    assert!(controller.borrow().task.is_none());
    assert_eq!(window.get_current_path(), original);
    assert_eq!(
        window.get_details().row_data(0).unwrap().node_id,
        details.node_id
    );
    for invalid in [-1, 1, 999] {
        window.invoke_select_volume(invalid);
        assert_eq!(window.get_selected_volume(), 0);
    }
    window.set_busy(true);
    window.invoke_select_volume(2);
    assert_eq!(window.get_selected_volume(), 0);
    window.set_busy(false);
    window.set_choosing_folder(true);
    window.invoke_select_volume(2);
    assert_eq!(window.get_selected_volume(), 0);
    window.set_choosing_folder(false);
    window.set_scan_enabled(false);
    window.invoke_select_volume(2);
    assert_eq!(window.get_selected_volume(), 0);
    window.set_scan_enabled(true);
    window.invoke_select_volume(2);
    assert!(!window.get_busy());
    assert!(controller.borrow().task.is_none());
    window.invoke_start_scan();
    wait(&window, &controller);
    assert_eq!(fs::read(fixture.path().join("file")).unwrap(), b"read only");
    assert!(window.get_summary().contains("1 fichiers"));
    window.invoke_path_edited();
    assert_eq!(window.get_selected_volume(), -1);
}
#[test]
fn source_projection_is_capped_and_unknown_is_not_zero() {
    let paths: Vec<_> = (0..300)
        .map(|i| std::path::PathBuf::from(format!("source-{i}")))
        .collect();
    let prepared = crate::volume_view::Prepared::new(discovered(&paths));
    assert!(prepared.limited);
    assert_eq!(prepared.rows.len(), 256);
    assert_eq!(prepared.discovery.volumes.len(), 256);
    let row = &prepared.rows[0];
    assert!(!row.has_usage);
    assert!(row.summary.contains("Inconnu"));
    assert!(!row.summary.contains("0 octets"));
}
