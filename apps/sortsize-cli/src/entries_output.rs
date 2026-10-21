//! Lazy staging: create after acquisition, publish only after successful flush.
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub(super) struct StagedOutput {
    destination: PathBuf,
    stage: Option<NamedTempFile>,
    #[cfg(test)]
    failure: Option<(&'static str, usize)>,
}
impl StagedOutput {
    pub fn new(destination: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Le fichier de destination existe déjà",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            destination: destination.into(),
            stage: None,
            #[cfg(test)]
            failure: None,
        })
    }
    #[cfg(test)]
    fn check(&mut self, operation: &'static str) -> io::Result<()> {
        if let Some((selected, remaining)) = &mut self.failure
            && *selected == operation
        {
            if *remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::StorageFull, operation));
            }
            *remaining -= 1;
        }
        Ok(())
    }
    pub fn publish(self) -> io::Result<()> {
        #[cfg(test)]
        if self.failure.is_some_and(|(point, _)| point == "sync") {
            return Err(io::Error::new(io::ErrorKind::StorageFull, "sync"));
        }
        let file = self
            .stage
            .ok_or_else(|| io::Error::other("Export sans fichier de staging"))?;
        file.as_file().sync_all()?;
        #[cfg(test)]
        if self.failure.is_some_and(|(point, _)| point == "publish") {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publish"));
        }
        // persist_noclobber never replaces a file/symlink created in the meantime.
        // This is not a promise of power-loss durability or universal atomicity.
        file.persist_noclobber(self.destination)
            .map_err(|e| e.error)?;
        Ok(())
    }
}
impl Write for StagedOutput {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        self.check("write")?;
        if self.stage.is_none() {
            #[cfg(test)]
            self.check("create")?;
            let parent = self
                .destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            self.stage = Some(
                tempfile::Builder::new()
                    .prefix(".sortsize-export-")
                    .tempfile_in(parent)?,
            );
        }
        match &mut self.stage {
            Some(file) => file.write(data),
            None => Err(io::Error::other("Staging indisponible")),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        #[cfg(test)]
        self.check("flush")?;
        match &mut self.stage {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destination_race_never_overwrites_and_drops_staging() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("out.csv");
        let mut writer = StagedOutput::new(&destination).unwrap();
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        writer.write_all(b"prepared").unwrap();
        writer.flush().unwrap();
        fs::write(&destination, b"competitor").unwrap();
        assert!(writer.publish().is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"competitor");
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    }
}

#[cfg(test)]
#[test]
fn failures_in_staging_never_publish_a_partial_file() {
    for point in ["create", "write", "flush", "sync", "publish"] {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("inventory.csv");
        let mut writer = StagedOutput::new(&destination).unwrap();
        writer.failure = Some((point, usize::from(point == "write")));
        let result = (|| {
            writer.write_all(b"header\npartial data\n")?;
            writer.write_all(b"end\n")?;
            writer.flush()?;
            writer.publish()
        })();
        assert!(result.is_err(), "{point}");
        assert!(!destination.exists(), "{point}");
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0, "{point}");
        parent.close().unwrap();
    }
}

#[cfg(test)]
#[test]
fn staging_parent_disappears_and_destination_becomes_directory() {
    let parent = tempfile::tempdir().unwrap();
    let vanished = parent.path().join("vanished");
    fs::create_dir(&vanished).unwrap();
    let mut writer = StagedOutput::new(&vanished.join("out")).unwrap();
    fs::remove_dir(&vanished).unwrap();
    assert_eq!(
        writer.write_all(b"data").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    drop(writer);
    let destination = parent.path().join("directory");
    let mut writer = StagedOutput::new(&destination).unwrap();
    writer.write_all(b"data").unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(writer.publish().is_err());
    assert!(destination.is_dir());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
}
