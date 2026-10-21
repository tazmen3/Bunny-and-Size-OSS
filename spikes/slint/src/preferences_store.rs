//! Desktop-only configuration I/O, isolated from scan and UI callbacks.
use crate::display_preferences::Preferences;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};
const MAX_CONFIG_BYTES: u64 = 16 * 1024;

pub(crate) fn config_path() -> Option<PathBuf> {
    resolve_path(|name| std::env::var_os(name))
}
fn resolve_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |name| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    #[cfg(windows)]
    let base = absolute("APPDATA");
    #[cfg(not(windows))]
    let base = absolute("XDG_CONFIG_HOME").or_else(|| absolute("HOME").map(|p| p.join(".config")));
    base.map(|p| p.join("sortsize").join("display-v1.conf"))
}
pub(crate) fn load(path: &Path) -> (Preferences, i32) {
    let read = || -> io::Result<String> {
        let mut text = String::new();
        fs::File::open(path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(text)
    };
    match read() {
        Ok(text) => match Preferences::decode(&text) {
            Some(p) => (p, 0),
            None => (Preferences::default(), 4),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound && missing_is_normal(path) => {
            (Preferences::default(), 0)
        }
        Err(_) => (Preferences::default(), 5),
    }
}
fn missing_is_normal(path: &Path) -> bool {
    // Windows reports ERROR_PATH_NOT_FOUND both for missing directories and for
    // a regular file used as an ancestor. Only the former is a normal first run.
    for parent in path.ancestors().skip(1) {
        match fs::metadata(parent) {
            Ok(metadata) => return metadata.is_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    true
}
pub(crate) fn save(path: &Path, preferences: &Preferences) -> io::Result<()> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    // Same-directory temporary file: persist atomically replaces on Linux and
    // Windows, never remove the destination before replacement. Private mode 0600.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(preferences.encode().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[derive(Default)]
struct Pending {
    latest: Option<(u64, Preferences)>,
    result: Option<(u64, bool)>,
    closing: bool,
}
pub(crate) struct Store {
    state: Arc<(Mutex<Pending>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
    serial: u64,
}
impl Store {
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let state = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let shared = state.clone();
        let worker = std::thread::Builder::new()
            .name("sortsize-preferences".into())
            .spawn(move || {
                loop {
                    let (lock, changed) = &*shared;
                    let (serial, preferences) = {
                        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        let mut state = changed
                            .wait_while(state, |s| s.latest.is_none() && !s.closing)
                            .unwrap_or_else(|e| e.into_inner());
                        match state.latest.take() {
                            Some(value) => value,
                            None => break,
                        }
                    };
                    let success = save(&path, &preferences).is_ok();
                    lock.lock().unwrap_or_else(|e| e.into_inner()).result = Some((serial, success));
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
            serial: 0,
        })
    }
    pub fn submit(&mut self, preferences: Preferences) {
        self.serial = self.serial.wrapping_add(1);
        self.state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest = Some((self.serial, preferences));
        self.state.1.notify_one();
    }
    pub fn poll(&self) -> Option<bool> {
        let mut state = self.state.0.try_lock().ok()?;
        state
            .result
            .take()
            .filter(|(serial, _)| *serial == self.serial)
            .map(|(_, success)| success)
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        // Only at application shutdown: flush the last accepted edit, never
        // wait/join on a settings callback. A newer pending edit wins.
        self.state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closing = true;
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn injected_path_load_atomic_replace_corruption_and_interruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/display-v1.conf");
        assert_eq!(load(&path).0, Preferences::default());
        let mut p = Preferences::default();
        p.set_column(10, false);
        p.units = crate::units::Units::Decimal;
        save(&path, &p).unwrap();
        assert_eq!(load(&path).0, p);
        // An interrupted write before persist must leave the last file intact.
        {
            let mut t = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
            t.write_all(b"version=").unwrap();
        }
        assert_eq!(load(&path).0, p);
        save(&path, &Preferences::default()).unwrap();
        assert_eq!(load(&path).0, Preferences::default());
        for text in [
            "corrupt",
            "version=999",
            &"x".repeat(MAX_CONFIG_BYTES as usize + 1),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(load(&path).0, Preferences::default());
            assert!(load(&path).1 != 0);
        }
        let blocked = dir.path().join("file");
        fs::write(&blocked, "not a directory").unwrap();
        assert!(save(&blocked.join("settings"), &p).is_err());
        assert!(load(&blocked.join("settings")).1 != 0);
        assert!(load(&blocked.join("nested/settings")).1 != 0);
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(load(&path).1, 5);
    }
    #[cfg(unix)]
    #[test]
    fn denied_permissions_are_nonfatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display-v1.conf");
        save(&path, &Preferences::default()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = save(&path, &Preferences::default());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            std::env::var("USER").as_deref() == Ok("root"),
            result.is_ok()
        );
    }
    #[test]
    fn rapid_changes_flush_latest_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display-v1.conf");
        let mut store = Store::start(path.clone()).unwrap();
        let mut p = Preferences::default();
        for i in 0..300 {
            p.set_column(3, i % 2 == 0);
            store.submit(p.clone());
        }
        p.units = crate::units::Units::Decimal;
        store.submit(p.clone());
        drop(store);
        assert_eq!(load(&path).0, p);
    }
    #[cfg(not(windows))]
    #[test]
    fn xdg_absolute_fallback_and_missing_environment_without_mutating_home() {
        let resolve = |xdg: Option<&str>, home: Option<&str>| {
            resolve_path(|key| match key {
                "XDG_CONFIG_HOME" => xdg.map(Into::into),
                "HOME" => home.map(Into::into),
                _ => None,
            })
        };
        assert_eq!(
            resolve(Some("/fixture/config"), Some("/fixture/home")).unwrap(),
            PathBuf::from("/fixture/config/sortsize/display-v1.conf")
        );
        assert_eq!(
            resolve(Some("relative"), Some("/fixture/home")).unwrap(),
            PathBuf::from("/fixture/home/.config/sortsize/display-v1.conf")
        );
        assert!(resolve(None, None).is_none());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    fn injected_roaming_appdata_is_absolute_and_does_not_use_program_files() {
        let path =
            super::resolve_path(|key| (key == "APPDATA").then(|| r"C:\fixture\Roaming".into()))
                .unwrap();
        assert_eq!(
            path,
            std::path::PathBuf::from(r"C:\fixture\Roaming\sortsize\display-v1.conf")
        );
        assert!(super::resolve_path(|_| Some("relative".into())).is_none());
    }
}
