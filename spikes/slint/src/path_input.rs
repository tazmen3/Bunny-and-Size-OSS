use std::path::PathBuf;

/// Slint displays UTF-8; only the native value may be used until a user edit.
#[derive(Default)]
pub(crate) struct PathInput {
    selected: Option<(PathBuf, String)>,
}

impl PathInput {
    pub fn select(&mut self, path: PathBuf) -> String {
        let display = path.to_string_lossy().into_owned();
        self.selected = Some((path, display.clone()));
        display
    }

    pub fn edited(&mut self) {
        self.selected = None;
    }

    pub fn resolve(&self, text: &str) -> PathBuf {
        match &self.selected {
            Some((path, display)) if display == text => path.clone(),
            _ => PathBuf::from(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_selection_and_manual_changes_are_distinct() {
        let mut input = PathInput::default();
        let path = PathBuf::from("été 日本語 avec espaces").join("long".repeat(100));
        let display = input.select(path.clone());
        assert_eq!(input.resolve(&display), path);
        assert_eq!(input.resolve("manual"), PathBuf::from("manual"));
        input.edited();
        assert_eq!(input.resolve(&display), PathBuf::from(display));
    }

    #[cfg(unix)]
    #[test]
    fn lossy_display_never_replaces_native_bytes_until_an_edit() {
        use std::os::unix::ffi::OsStringExt;
        let mut input = PathInput::default();
        let native = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/native-\xff".to_vec()));
        let display = input.select(native.clone());
        assert_eq!(input.resolve(&display), native);
        // An edit followed by undo to the same text is still manual input.
        input.edited();
        assert_ne!(input.resolve(&display), native);
        assert_eq!(input.resolve(&display), PathBuf::from(display));
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_utf16_survives_native_selection() {
        use std::os::windows::ffi::OsStringExt;
        let native = PathBuf::from(std::ffi::OsString::from_wide(&[67, 58, 92, 0xd800]));
        let mut input = PathInput::default();
        let display = input.select(native.clone());
        assert_eq!(input.resolve(&display), native);
        input.edited();
        assert_ne!(input.resolve(&display), native);
    }
}
