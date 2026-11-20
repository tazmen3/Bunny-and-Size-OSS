//! Delete one dated record using the existing owner and lease checks.
use super::*;
use sortsize_store::{Durability, inventory::generation_a::Repository};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};

pub(super) fn forget_inventory(
    saved: &Saved,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let repository = if saved.directory.join("inventory.db").exists() {
        Repository::resolve_legacy(&saved.directory, cancel)?
    } else {
        Some(saved.directory.clone())
    };
    let shared =
        tr!("This measure shares storage with other records and cannot be deleted separately.");
    if let Some(path) = repository {
        let repository = Repository::open_existing(&path)?;
        let descriptors = repository.descriptors()?;
        let [only] = descriptors.as_slice() else {
            return Err(shared.into());
        };
        if repository.legacy_origin()?.is_some() {
            return Err(shared.into());
        }
        if only.context != saved.context
            || only.root != saved.root
            || only.measured != saved.measured
            || only.started != saved.started
        {
            return Err("saved measure changed; refresh the catalogue".into());
        }
        let candidate = repository.candidate_path(only.generation);
        for entry in std::fs::read_dir(repository.directory())? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry.path() != only.directory
                && entry.path() != candidate
            {
                return Err(shared.into());
            }
        }
        repository.forget(cancel)?;
    } else {
        let store = Store::reopen(&saved.directory, Durability::Retained)?;
        let descriptors = store.saved_records()?;
        let [only] = descriptors.as_slice() else {
            return Err(shared.into());
        };
        if only.context != saved.context
            || only.root != saved.root
            || only.measured != saved.measured
            || only.started != saved.started
        {
            return Err("saved measure changed; refresh the catalogue".into());
        }
        if cancel.is_cancelled() {
            return Err("saved measure deletion cancelled".into());
        }
        store.forget()?;
    }
    Ok(())
}

pub(super) struct Journal {
    file: File,
    started: u128,
}

impl Journal {
    pub(super) fn open(root: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = root.with_extension("deletions.log");
        let parent = path.parent().ok_or("deletion journal has no parent")?;
        sortsize_core::acquisition::local_state_location(parent)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err("deletion journal reparse point".into());
                    }
                }
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err("deletion journal is not a regular file".into());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self {
            file,
            started: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        })
    }

    pub(super) fn record(&self, outcome: &str) -> io::Result<()> {
        let mut file = &self.file;
        file.lock()?;
        let result = (|| {
            if file.metadata()?.len() > 1024 * 1024 - 128 {
                file.set_len(0)?;
            }
            file.seek(SeekFrom::End(0))?;
            writeln!(file, "{} {} {}", self.started, std::process::id(), outcome)?;
            file.sync_data()
        })();
        let unlocked = file.unlock();
        result.and(unlocked)
    }
}
