#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeCoverage {
    Unknown,
    Partial,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingState {
    NotStarted,
    InProgress,
    Exhausted,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Pending,
    Complete,
}

/// Evidence is scoped to one directory, attempt, policy and metric by its
/// owning store. EOF does not certify reconciliation, descendants or totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryEvidence {
    pub listing: ListingState,
    pub reconciliation: Completion,
    pub descendants: ScopeCoverage,
    pub required_metadata: ScopeCoverage,
    pub aggregates: Completion,
}

impl DirectoryEvidence {
    pub fn coverage(self) -> ScopeCoverage {
        if self.listing == ListingState::NotStarted {
            ScopeCoverage::Unknown
        } else if self.listing == ListingState::Exhausted
            && self.reconciliation == Completion::Complete
            && self.descendants == ScopeCoverage::Complete
            && self.required_metadata == ScopeCoverage::Complete
            && self.aggregates == Completion::Complete
        {
            ScopeCoverage::Complete
        } else {
            ScopeCoverage::Partial
        }
    }

    /// Permits classifying names not seen in this attempt as absent from the
    /// comparable listing. This never proves physical deletion or free space.
    pub fn permits_absence_inference(self, comparison: ComparisonScope) -> bool {
        self.listing == ListingState::Exhausted && comparison == ComparisonScope::Comparable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonScope {
    Comparable,
    Uncertain,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    MeasuredThisEpoch,
    RecordedUnverified,
    Invalidated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationState {
    Live,
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingCoverage {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationState {
    InProgress,
    Complete,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultAvailability {
    /// All matches remain accessible, possibly through many pages.
    AllAccessible,
    Truncated,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCompleteness {
    Complete,
    Partial,
    Unavailable,
}

/// No RAM/page count or freshness field participates in completeness. A sealed
/// old measure can be complete in its scope and still be unverified today.
/// Store integrity must be checked before supplying usable recording evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryEvidence {
    pub source: ScopeCoverage,
    pub recording: RecordingCoverage,
    pub generation: GenerationState,
    pub evaluation: EvaluationState,
    pub unevaluable: u64,
    pub results: ResultAvailability,
}

impl QueryEvidence {
    pub fn completeness(self) -> QueryCompleteness {
        if self.recording == RecordingCoverage::Unavailable
            || self.results == ResultAvailability::Unavailable
        {
            QueryCompleteness::Unavailable
        } else if self.source == ScopeCoverage::Complete
            && self.recording == RecordingCoverage::Complete
            && self.generation == GenerationState::Sealed
            && self.evaluation == EvaluationState::Complete
            && self.unevaluable == 0
            && self.results == ResultAvailability::AllAccessible
        {
            QueryCompleteness::Complete
        } else {
            QueryCompleteness::Partial
        }
    }
}
