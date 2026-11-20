//! Bounded ancestor cache for a sealed generation; no source IO.
use super::*;
use std::collections::{BTreeMap, VecDeque};
#[derive(Clone)]
struct Located {
    path: Arc<PathBuf>,
    depth: usize,
}
#[derive(Default)]
pub(in crate::inventory) struct Paths {
    directories: BTreeMap<EntryId, Option<Located>>,
    order: VecDeque<EntryId>,
    bytes: usize,
    max_bytes: usize,
    row_path: PathBuf,
}
fn cost(p: &Option<Located>) -> usize {
    p.as_ref().map_or(0, |p| p.path.capacity()) + 256
}
fn push_component(path: &mut PathBuf, raw: &[u8]) -> Result<()> {
    #[cfg(unix)]
    if let [1, 0, bytes @ ..] = raw {
        use std::os::unix::ffi::OsStrExt;
        path.push(std::ffi::OsStr::from_bytes(bytes));
        return Ok(());
    }
    path.push(crate::scheduler::native_name(
        &NativeName::decode(raw).map_err(|_| Error::Contract("invalid name"))?,
    )?);
    Ok(())
}
fn append(parent: &Located, raw: &[u8]) -> Result<Located> {
    if parent.depth >= 1024 {
        return Err(Error::Contract("recorded path exceeds projection budget"));
    }
    let mut path = parent.path.as_ref().clone();
    push_component(&mut path, raw)?;
    if path.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
        return Err(Error::Contract("recorded path exceeds projection budget"));
    }
    Ok(Located {
        path: Arc::new(path),
        depth: parent.depth + 1,
    })
}
impl Paths {
    pub(in crate::inventory) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            ..Default::default()
        }
    }
    fn remember(&mut self, id: EntryId, path: Option<Located>) {
        if self.directories.contains_key(&id) {
            return;
        }
        let bytes = cost(&path);
        if bytes > self.max_bytes {
            return;
        }
        while self.directories.len() >= 256 || self.bytes + bytes > self.max_bytes {
            let old = self.order.pop_front().unwrap();
            self.bytes -= cost(&self.directories.remove(&old).unwrap());
        }
        self.bytes += bytes;
        self.order.push_back(id);
        self.directories.insert(id, path);
    }
    fn directory(
        &mut self,
        store: &Store,
        g: GenerationId,
        mut id: EntryId,
        root: &Path,
    ) -> Result<Option<Located>> {
        let mut pending: Vec<(EntryId, Vec<u8>)> = Vec::new();
        let mut bytes = root.as_os_str().as_encoded_bytes().len();
        let mut path;
        loop {
            if let Some(p) = self.directories.get(&id) {
                path = p.clone();
                break;
            }
            let mut stmt = store.db.prepare(
                "SELECT parent,name,kind,present FROM entries WHERE generation=?1 AND id=?2",
            )?;
            let (parent, name, kind, present) =
                stmt.query_row(params![unsigned(g.0), unsigned(id.0)], |r| {
                    Ok((
                        opt_number(r, 0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })?;
            if !present || kind != Kind::Directory as i64 {
                self.remember(id, None);
                return Ok(None);
            }
            if id.0 == 0 {
                if bytes > 32 * 1024 {
                    return Err(Error::Contract("recorded root exceeds projection budget"));
                }
                path = Some(Located {
                    path: Arc::new(root.to_owned()),
                    depth: 0,
                });
                self.remember(id, path.clone());
                break;
            }
            let parent = parent
                .filter(|p| *p < id.0)
                .ok_or(Error::Contract("invalid recorded ancestor"))?;
            bytes += name.len();
            if bytes > 32 * 1024 || pending.len() >= 1024 {
                return Err(Error::Contract("recorded path exceeds projection budget"));
            }
            pending.push((id, name));
            id = EntryId(parent);
        }
        for (id, name) in pending.into_iter().rev() {
            path = path.as_ref().map(|p| append(p, &name)).transpose()?;
            self.remember(id, path.clone());
        }
        Ok(path)
    }
    pub(in crate::inventory) fn row(
        &mut self,
        store: &Store,
        g: GenerationId,
        id: EntryId,
        root: &Path,
        row: &rusqlite::Row<'_>,
    ) -> Result<Option<&Path>> {
        let present: bool = row.get(7)?;
        if !present {
            return Ok(None);
        }
        let kind: i64 = row.get(1)?;
        if id.0 == 0 {
            let Some(located) = self.directory(store, g, id, root)? else {
                return Ok(None);
            };
            self.row_path.clone_from(located.path.as_ref());
            return Ok(Some(&self.row_path));
        }
        let parent = opt_number(row, 5)?
            .filter(|p| *p < id.0)
            .ok_or(Error::Contract("invalid recorded ancestor"))?;
        let Some(parent) = self.directory(store, g, EntryId(parent), root)? else {
            return Ok(None);
        };
        let raw = row.get_ref(6)?;
        let raw = raw.as_blob().map_err(rusqlite::Error::from)?;
        if parent.depth >= 1024 {
            return Err(Error::Contract("recorded path exceeds projection budget"));
        }
        self.row_path.clone_from(parent.path.as_ref());
        push_component(&mut self.row_path, raw)?;
        if self.row_path.as_os_str().as_encoded_bytes().len() > 32 * 1024 {
            self.row_path = PathBuf::new();
            return Err(Error::Contract("recorded path exceeds projection budget"));
        }
        if kind == Kind::Directory as i64 {
            self.remember(
                id,
                Some(Located {
                    path: Arc::new(self.row_path.clone()),
                    depth: parent.depth + 1,
                }),
            );
        }
        Ok(Some(&self.row_path))
    }
}
