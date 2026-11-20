use super::{
    GenerationId, InventoryId, PresentationEpoch, QueryId, Revision, ScanId, SourceId,
    ValidationEpoch, ViewEpoch, ViewId,
};

pub const PAGE_ROWS: usize = 200;
pub const TOP_ROWS: usize = 100;
pub const TREEMAP_TILES: usize = 256;
pub const PUBLICATION_BYTES: usize = 512 * 1024;

/// Local database views do not require a live mount or an acquisition grant.
/// A new sort, page or scope changes ViewEpoch; presentation changes have their
/// own epoch. IDs are never converted into Slint's page-local row indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewContext {
    pub inventory: InventoryId,
    pub generation: GenerationId,
    pub source: SourceId,
    pub scan: ScanId,
    pub validation: ValidationEpoch,
    pub view: ViewId,
    pub epoch: ViewEpoch,
    pub query: Option<QueryId>,
    pub presentation: PresentationEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewStamp {
    pub context: ViewContext,
    pub revision: Revision,
}

/// Pure acceptance guard for prepared publications. Byte/row validation and
/// page-token-to-entry mapping are the projection adapter's responsibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewGuard {
    expected: ViewContext,
    applied: Option<Revision>,
}

impl ViewGuard {
    pub fn new(expected: ViewContext) -> Self {
        Self {
            expected,
            applied: None,
        }
    }

    pub fn applied(self) -> Option<Revision> {
        self.applied
    }

    pub fn accept_snapshot(&mut self, stamp: ViewStamp) -> bool {
        if stamp.context != self.expected || self.applied.is_some_and(|r| stamp.revision <= r) {
            return false;
        }
        self.applied = Some(stamp.revision);
        true
    }

    pub fn accept_delta(&mut self, base: Revision, stamp: ViewStamp) -> bool {
        if self.applied != Some(base) || stamp.revision <= base {
            return false;
        }
        self.accept_snapshot(stamp)
    }
}
