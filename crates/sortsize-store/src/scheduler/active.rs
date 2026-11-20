//! Process-wide exclusion for roots already being acquired. The lease follows
//! blocked native jobs even if their Session is dropped. No filesystem calls.
use super::*;
use std::sync::Weak;
#[derive(Debug)]
pub(super) struct ActiveRoot {
    root: PathBuf,
    identity: Option<sortsize_core::LocalFileId>,
}
pub(super) fn acquire(guard: &Guard) -> Result<Arc<ActiveRoot>> {
    if crate::inventory::queries::workspace::adaptive::is_active_stamp(guard.evidence().1) {
        return Err(Error::Contract(
            "selected source is an active duplicate workspace",
        ));
    }
    static ACTIVE: OnceLock<Mutex<Vec<Weak<ActiveRoot>>>> = OnceLock::new();
    let mut active = ACTIVE.get_or_init(Mutex::default).lock().unwrap();
    active.retain(|a| a.strong_count() > 0);
    let root = ActiveRoot {
        root: guard.root().to_owned(),
        identity: guard.evidence().1.identity,
    };
    for entry in active.iter().filter_map(Weak::upgrade) {
        if root.root.starts_with(&entry.root)
            || entry.root.starts_with(&root.root)
            || (root.identity.is_some() && root.identity == entry.identity)
        {
            return Err(Error::Contract(
                "source root already has an active acquisition",
            ));
        }
    }
    if active.len() >= 16 {
        return Err(Error::Contract("active source registry budget"));
    }
    let root = Arc::new(root);
    active.push(Arc::downgrade(&root));
    Ok(root)
}
