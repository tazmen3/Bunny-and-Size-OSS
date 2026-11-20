/// Stable preference keys. An unknown key should fall back to Default in the
/// preference adapter; no translated label or ComboBox position is persisted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MemoryMode {
    #[default]
    Auto,
    Entries50k,
    Entries250k,
    Entries1m,
    Entries5m,
    UnlimitedCount,
}

impl MemoryMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Entries50k => "50000",
            Self::Entries250k => "250000",
            Self::Entries1m => "1000000",
            Self::Entries5m => "5000000",
            Self::UnlimitedCount => "unlimited-count",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "auto" => Some(Self::Auto),
            "50000" => Some(Self::Entries50k),
            "250000" => Some(Self::Entries250k),
            "1000000" => Some(Self::Entries1m),
            "5000000" => Some(Self::Entries5m),
            "unlimited-count" => Some(Self::UnlimitedCount),
            _ => None,
        }
    }

    /// Auto's estimate comes from the future measured byte governor. Zero is
    /// valid under resource pressure; there is no mandatory minimum cache.
    pub fn entry_limit(self, auto_estimate: u64) -> Option<u64> {
        match self {
            Self::Auto => Some(auto_estimate),
            Self::Entries50k => Some(50_000),
            Self::Entries250k => Some(250_000),
            Self::Entries1m => Some(1_000_000),
            Self::Entries5m => Some(5_000_000),
            Self::UnlimitedCount => None,
        }
    }
}

/// An admission predicate, not an allocator or a process RSS guarantee.
/// Used sizes include pinned pages and allocated capacities, not just lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheBudget {
    pub entries: Option<u64>,
    pub bytes: u64,
}

impl CacheBudget {
    pub fn admits(self, used_entries: u64, used_bytes: u64, entries: u64, bytes: u64) -> bool {
        let Some(total_entries) = used_entries.checked_add(entries) else {
            return false;
        };
        let Some(total_bytes) = used_bytes.checked_add(bytes) else {
            return false;
        };
        total_bytes <= self.bytes && self.entries.is_none_or(|limit| total_entries <= limit)
    }
}
