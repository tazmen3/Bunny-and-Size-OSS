//! IDs are assigned by the owning service/store, never by a UI row index.

macro_rules! identifier {
    ($name:ident, $value:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub $value);
    };
}

identifier!(InventoryId, u128);
identifier!(SourceId, u128);
identifier!(GenerationId, u64);
identifier!(ScanId, u64);
identifier!(EntryId, u64);
identifier!(PolicyId, u64);
identifier!(MountEpoch, u64);
identifier!(RunEpoch, u64);
identifier!(ValidationEpoch, u64);
identifier!(AttemptId, u64);
identifier!(OwnerEpoch, u64);
identifier!(BranchRevision, u64);
identifier!(ViewId, u64);
identifier!(ViewEpoch, u64);
identifier!(QueryId, u64);
identifier!(PresentationEpoch, u64);
identifier!(Revision, u64);

/// An entry occurrence, not a physical file ID. Hardlink aliases have distinct
/// keys; correspondence between generations needs separate qualified evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryKey {
    pub inventory: InventoryId,
    pub generation: GenerationId,
    pub entry: EntryId,
}

/// Context for an authorized acquisition. Equality is necessary for ingestion,
/// but is not itself authorization or proof of native source identity.
/// PolicyId references immutable policy data within this inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AcquisitionContext {
    pub inventory: InventoryId,
    pub generation: GenerationId,
    pub source: SourceId,
    pub scan: ScanId,
    pub policy: PolicyId,
    pub mount: MountEpoch,
    pub run: RunEpoch,
    pub validation: ValidationEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    EnumerateDirect,
    Reconcile,
    Consolidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskKey {
    pub context: AcquisitionContext,
    pub directory: EntryId,
    pub kind: TaskKind,
}

/// Fencing tuple. The store must check that this is still the active owner and
/// branch revision, including before acknowledging a replay of an old batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptKey {
    pub task: TaskKey,
    pub attempt: AttemptId,
    pub owner: OwnerEpoch,
    pub branch: BranchRevision,
}
