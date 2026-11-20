use super::AttemptKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingEnd {
    Exhausted,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchHeader {
    pub attempt: AttemptKey,
    pub sequence: u64,
    /// Native listing outcome, not reconciliation or recursive completion.
    pub end: Option<ListingEnd>,
}

/// Pure sequencing state for one active direct-listing attempt. The future
/// store persists this together with entries, contributions and child tasks.
/// It is not a durable writer or a scheduler on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCursor {
    attempt: AttemptKey,
    next_sequence: u64,
    end: Option<ListingEnd>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDecision {
    /// A committed sequence: acknowledge without evaluating/reapplying payload.
    Replay,
    /// Persist this state atomically with the payload, then publish the ack.
    Apply { next: BatchCursor },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRejection {
    StaleAttempt,
    SequenceGap,
    ListingAlreadyEnded,
    SequenceExhausted,
}

impl BatchCursor {
    pub fn start(attempt: AttemptKey) -> Self {
        Self {
            attempt,
            next_sequence: 0,
            end: None,
        }
    }

    /// Restore a cursor read from one committed store transaction. The store
    /// still validates ownership and branch fencing before prepare/replay.
    pub fn from_committed(
        attempt: AttemptKey,
        next_sequence: u64,
        end: Option<ListingEnd>,
    ) -> Option<Self> {
        if next_sequence == 0 && end.is_some() {
            return None;
        }
        Some(Self {
            attempt,
            next_sequence,
            end,
        })
    }

    pub fn next_sequence(self) -> u64 {
        self.next_sequence
    }

    pub fn listing_end(self) -> Option<ListingEnd> {
        self.end
    }

    /// Does not mutate the committed cursor. If the store transaction fails,
    /// discard the returned candidate and retry against the old cursor.
    /// Before calling, the store must check active ownership/branch revocation.
    /// Replayed headers are identifiers only; their payload is never reapplied.
    pub fn prepare(self, batch: BatchHeader) -> Result<BatchDecision, BatchRejection> {
        if batch.attempt != self.attempt {
            return Err(BatchRejection::StaleAttempt);
        }
        if batch.sequence < self.next_sequence {
            return Ok(BatchDecision::Replay);
        }
        if self.end.is_some() {
            return Err(BatchRejection::ListingAlreadyEnded);
        }
        if batch.sequence != self.next_sequence {
            return Err(BatchRejection::SequenceGap);
        }
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(BatchRejection::SequenceExhausted)?;
        Ok(BatchDecision::Apply {
            next: Self {
                attempt: self.attempt,
                next_sequence,
                end: batch.end,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory_contract::*;

    #[test]
    fn sequence_exhaustion_cannot_wrap_to_zero() {
        let attempt = AttemptKey {
            task: TaskKey {
                context: AcquisitionContext {
                    inventory: InventoryId(1),
                    generation: GenerationId(1),
                    source: SourceId(1),
                    scan: ScanId(1),
                    policy: PolicyId(1),
                    mount: MountEpoch(1),
                    run: RunEpoch(1),
                    validation: ValidationEpoch(1),
                },
                directory: EntryId(1),
                kind: TaskKind::EnumerateDirect,
            },
            attempt: AttemptId(1),
            owner: OwnerEpoch(1),
            branch: BranchRevision(1),
        };
        let cursor = BatchCursor {
            attempt,
            next_sequence: u64::MAX,
            end: None,
        };
        assert_eq!(
            cursor.prepare(BatchHeader {
                attempt,
                sequence: u64::MAX,
                end: None
            }),
            Err(BatchRejection::SequenceExhausted)
        );
        assert_eq!(cursor.next_sequence(), u64::MAX);
    }
}
