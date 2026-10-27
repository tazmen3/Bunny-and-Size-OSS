//! Validate a native navigation target without deriving it from a UI label.
use crate::ScanFailure;
use std::path::{Component, Path};

/// Target must be a real directory within the original canonical scan root.
/// Reject symlinks/reparse points in every component, including replacements
/// since the original scan. Like traversal, this is not an atomic FS snapshot;
/// a concurrent rename/replacement after validation remains a native I/O race.
pub fn validate_directory_scope(root: &Path, target: &Path) -> Result<(), ScanFailure> {
    // canonicalize(UNC share) may return only its prefix, whereas the parent
    // of a file at that share has a trailing root separator. Align these two
    // native spellings before strip_prefix; never normalize drive-relative C:.
    #[cfg(windows)]
    let normalized_root = normalize_share_root(root);
    #[cfg(windows)]
    let root = normalized_root.as_deref().unwrap_or(root);
    #[cfg(windows)]
    let normalized_target = normalize_share_root(target);
    #[cfg(windows)]
    let target = normalized_target.as_deref().unwrap_or(target);
    if !root.is_absolute() {
        return Err(ScanFailure::InvalidRoot(root.into()));
    }
    let relative = target
        .strip_prefix(root)
        .map_err(|_| ScanFailure::InvalidRoot(target.into()))?;
    let mut path = root.to_path_buf();
    // The root was canonical at scan time, but an ancestor above it may have
    // since been replaced by a symlink/junction. Checking only root itself
    // would follow that intermediate link before enumerating the target.
    for ancestor in root.ancestors() {
        check(ancestor)?;
    }
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ScanFailure::InvalidRoot(target.into()));
        };
        path.push(name);
        check(&path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn normalize_share_root(path: &Path) -> Option<std::path::PathBuf> {
    use std::path::Prefix;
    let mut components = path.components();
    if let Some(Component::Prefix(prefix)) = components.next()
        && matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
        && components.next().is_none()
    {
        let mut native = path.as_os_str().to_owned();
        native.push("\\");
        return Some(native.into());
    }
    None
}
fn check(path: &Path) -> Result<(), ScanFailure> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| ScanFailure::RootIo {
        path: path.into(),
        error,
    })?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse {
        return Err(ScanFailure::InvalidRoot(path.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn replaced_ancestor_above_the_original_root_cannot_redirect_a_listing() {
        let fixture = tempfile::TempDir::new().unwrap();
        let original_parent = fixture.path().join("parent");
        let replacement_parent = fixture.path().join("replacement");
        std::fs::create_dir_all(original_parent.join("root/child")).unwrap();
        std::fs::create_dir_all(replacement_parent.join("root/child")).unwrap();
        let root = std::fs::canonicalize(original_parent.join("root")).unwrap();
        std::fs::rename(&original_parent, fixture.path().join("saved-parent")).unwrap();
        std::os::unix::fs::symlink(&replacement_parent, &original_parent).unwrap();
        assert!(matches!(
            validate_directory_scope(&root, &root.join("child")),
            Err(ScanFailure::InvalidRoot(_))
        ));
    }

    #[test]
    fn scope_rejects_escape_files_disappearance_and_replaced_ancestors() {
        let fixture = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        std::fs::create_dir(root.join("child")).unwrap();
        std::fs::write(root.join("file"), [0]).unwrap();
        assert!(validate_directory_scope(&root, &root.join("child")).is_ok());
        for path in [
            root.join("../escape"),
            other.path().into(),
            root.join("file"),
            root.join("missing"),
        ] {
            assert!(validate_directory_scope(&root, &path).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(other.path(), root.join("link")).unwrap();
            std::fs::create_dir(other.path().join("inner")).unwrap();
            assert!(validate_directory_scope(&root, &root.join("link/inner")).is_err());
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn share_root_spellings_align_without_accepting_drive_relative_paths() {
        for (bare, rooted) in [
            (r"\\server\share", r"\\server\share\"),
            (r"\\?\UNC\server\été 日本語", r"\\?\UNC\server\été 日本語\"),
        ] {
            let normalized = normalize_share_root(Path::new(bare))
                .unwrap_or_else(|| std::path::PathBuf::from(bare));
            assert!(normalized.is_absolute());
            assert_eq!(
                Path::new(rooted).strip_prefix(&normalized).unwrap(),
                Path::new("")
            );
            assert_eq!(
                normalized.join("child").strip_prefix(&normalized).unwrap(),
                Path::new("child")
            );
        }
        for unchanged in [r"C:", r"\\?\C:", r"C:\", r"\\server\share\child"] {
            assert!(normalize_share_root(Path::new(unchanged)).is_none());
        }
    }
    #[test]
    fn a_replaced_junction_above_the_original_root_is_rejected() {
        let fixture = tempfile::TempDir::new().unwrap();
        let parent = fixture.path().join("parent");
        let replacement = fixture.path().join("replacement");
        std::fs::create_dir_all(parent.join("root/child")).unwrap();
        std::fs::create_dir_all(replacement.join("root/child")).unwrap();
        let root = std::fs::canonicalize(parent.join("root")).unwrap();
        std::fs::rename(&parent, fixture.path().join("saved-parent")).unwrap();
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&parent)
            .arg(&replacement)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "native junction fixture creation failed"
        );
        let result = validate_directory_scope(&root, &root.join("child"));
        std::fs::remove_dir(&parent).unwrap();
        assert!(matches!(result, Err(ScanFailure::InvalidRoot(_))));
        assert!(replacement.join("root/child").is_dir());
    }

    #[test]
    fn a_native_junction_cannot_redirect_a_complementary_scan() {
        let fixture = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(other.path().join("inner")).unwrap();
        let link = fixture.path().join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(other.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "native junction fixture creation failed"
        );
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let result = validate_directory_scope(&root, &root.join("junction").join("inner"));
        std::fs::remove_dir(&link).unwrap(); // Remove only the test junction.
        assert!(matches!(result, Err(ScanFailure::InvalidRoot(_))));
        assert!(other.path().join("inner").is_dir());
    }
}
