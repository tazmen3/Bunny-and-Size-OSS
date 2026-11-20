//! Single-entry writes authorized from the Windows Shell menu.
use super::{ShellAllocation, shell_item, shell_path};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, HANDLE},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandleEx,
            MOVE_FILE_FLAGS, MoveFileExW,
        },
        UI::Shell::{
            BHID_Transfer, FOLDERID_RecycleBinFolder, IShellItem, ITransferSource,
            SHCreateItemFromIDList, SHGetKnownFolderIDList, TSF_NORMAL,
        },
    },
    core::{Error, PCWSTR, Result},
};

pub(crate) struct Target {
    path: PathBuf,
    original: File,
    directory: bool,
}

impl Target {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        let original = open_entry(path)?;
        let metadata = original.metadata()?;
        // Leave reparse points and roots to the Explorer. The Shell provider
        // may apply semantics other than a simple file-system operation.
        if path.file_name().is_none()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(E_INVALIDARG.into());
        }
        file_id(&original)?;
        Ok(Self {
            path: path.to_owned(),
            original,
            directory: metadata.is_dir(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn is_directory(&self) -> bool {
        self.directory
    }

    fn revalidate(&self) -> Result<()> {
        let current = open_entry(&self.path)?;
        if file_id(&self.original)? != file_id(&current)?
            || current.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }

    pub(crate) fn rename(&self, name: &str) -> Result<()> {
        if !valid_name(name) {
            return Err(E_INVALIDARG.into());
        }
        let source: Vec<_> = self.path.as_os_str().encode_wide().chain([0]).collect();
        let destination: Vec<_> = self
            .path
            .with_file_name(name)
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        self.revalidate()?;
        // SAFETY: both names are native terminated paths. Zero flags prevents
        // replacement of an existing entry or copy/delete across volumes.
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                MOVE_FILE_FLAGS(0),
            )
        }
    }

    pub(super) fn recycle(&self) -> Result<bool> {
        let wide = shell_path(&self.path)?;
        let source = shell_item(&self.path, &wide)?;
        // SAFETY: the caller holds OLE initialized on the UI thread. Both
        // Shell allocations outlive their use to create the corresponding items.
        unsafe {
            let item: IShellItem = SHCreateItemFromIDList(source.0)?;
            let parent = item.GetParent()?;
            let transfer: ITransferSource = parent.BindToHandler(None, &BHID_Transfer)?;
            let recycle =
                ShellAllocation(SHGetKnownFolderIDList(&FOLDERID_RecycleBinFolder, 0, None)?);
            let recycle: IShellItem = SHCreateItemFromIDList(recycle.0)?;
            let journal = Journal::open()?;
            journal.record("started")?;
            let result = self.revalidate().and_then(|()| {
                // RecycleItem has an explicit destination and returns the
                // recycled item. There is no call to RemoveItem or delete verb.
                transfer.RecycleItem(&item, &recycle, TSF_NORMAL.0 as u32)
            });
            let recorded = journal.record(if result.is_ok() { "recycled" } else { "failed" });
            result.map(|_| recorded.is_ok())
        }
    }
}

fn open_entry(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

fn file_id(file: &File) -> Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the borrowed file handle is valid and the output is sized and writable.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )?;
    }
    let id = info.FileId.Identifier;
    if id == [0; 16] {
        return Err(E_INVALIDARG.into());
    }
    Ok((info.VolumeSerialNumber, id))
}

fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c <= '\u{1f}' || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

struct Journal {
    file: File,
    time: u128,
}

impl Journal {
    fn open() -> Result<Self> {
        let directory = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(E_INVALIDARG)?
            .join("sortsize");
        fs::create_dir_all(&directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("windows-recycle-v1.log"))?;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Ok(Self { file, time })
    }

    fn record(&self, event: &str) -> Result<()> {
        const MAX_BYTES: u64 = 1024 * 1024;
        let mut file = &self.file;
        file.lock()?;
        let result = (|| -> io::Result<()> {
            if file.metadata()?.len() > MAX_BYTES - 128 {
                file.set_len(0)?;
            }
            file.seek(SeekFrom::End(0))?;
            writeln!(
                file,
                "{} {} recycle {}",
                self.time,
                std::process::id(),
                event
            )?;
            file.sync_data()
        })();
        let unlocked = file.unlock();
        result.and(unlocked).map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_checks_identity_and_preserves_an_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Cible été.txt");
        let neighbor = temp.path().join("Témoin.txt");
        fs::write(&path, "original").unwrap();
        fs::write(&neighbor, "neighbor").unwrap();
        let target = Target::capture(&path).unwrap();
        assert!(target.rename("Témoin.txt").is_err());
        assert_eq!(fs::read_to_string(neighbor).unwrap(), "neighbor");
        assert!(target.rename("../elsewhere").is_err());
        assert!(target.rename("NUL.txt").is_err());
        target.rename("Nouveau été.txt").unwrap();
        fs::write(&path, "replacement").unwrap();
        assert!(target.rename("wrong.txt").is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement");
        assert_eq!(
            fs::read_to_string(temp.path().join("Nouveau été.txt")).unwrap(),
            "original"
        );
    }
}
