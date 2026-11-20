//! Owned local read capability, including the historical monolithic format.
use sortsize_core::{CancellationToken, analysis::SearchQuery, inventory_contract::GenerationId};
use sortsize_store::inventory::{
    Store,
    generation_a::{CertifiedGeneration, repository::Repository},
    queries::Query,
};
use std::path::{Path, PathBuf};

pub(crate) enum View {
    Legacy(Store),
    Candidate(Store),
    Certified(CertifiedGeneration),
}
pub(crate) fn is_repository(path: &Path) -> bool {
    ["IDENTITY.json", "CURRENT.json", "STATE.json"]
        .iter()
        .any(|name| std::fs::symlink_metadata(path.join(name)).is_ok())
}
/// Resolving an explicitly adopted legacy origin is local and fail-closed.
pub(crate) fn resolved_path(
    path: &Path,
    cancel: &CancellationToken,
) -> sortsize_store::inventory::Result<PathBuf> {
    if is_repository(path) {
        return Ok(path.to_owned());
    }
    Ok(Repository::resolve_legacy(path, cancel)?.unwrap_or_else(|| path.to_owned()))
}
impl View {
    pub(crate) fn open(
        path: &Path,
        generation: GenerationId,
        cancel: &CancellationToken,
    ) -> sortsize_store::inventory::Result<Self> {
        let path = resolved_path(path, cancel)?;
        if is_repository(&path) {
            let repository = Repository::open_readonly(&path)?;
            if repository.published(generation)?.is_some() {
                Ok(Self::Certified(
                    repository.open_generation(generation, cancel)?,
                ))
            } else {
                Ok(Self::Candidate(Store::certify_readonly(
                    &repository.candidate_path(generation),
                    cancel,
                )?))
            }
        } else {
            Ok(Self::Legacy(Store::certify_readonly(&path, cancel)?))
        }
    }
    pub(crate) fn store(&self) -> &Store {
        match self {
            Self::Legacy(store) | Self::Candidate(store) => store,
            Self::Certified(generation) => generation.as_store(),
        }
    }
    pub(crate) fn query<'a>(
        &'a self,
        generation: GenerationId,
        root: &Path,
        query: &SearchQuery,
        temporary: &Path,
        max_bytes: u64,
        cancel: &CancellationToken,
    ) -> sortsize_store::inventory::Result<Query<'a>> {
        match self {
            Self::Certified(capability) if capability.root() == root => {
                capability.query(query, temporary, max_bytes, cancel)
            }
            // Root-relative path predicates retain their historical caller-root
            // semantics. A coverage proof bound to another root is not reused.
            _ => self
                .store()
                .query(generation, root, query, temporary, max_bytes, cancel),
        }
    }
}
