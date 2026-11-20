//! Optional native properties, committed with observations, never acquired while reading.
use super::*;
use sortsize_core::{MetadataValue, NativeOwnerId, ValueAccuracy, acquisition::NodeProperties};
use std::time::{SystemTime, UNIX_EPOCH};
fn state<T>(v: &MetadataValue<T>) -> i64 {
    match v {
        MetadataValue::NotCollected => 0,
        MetadataValue::Known {
            accuracy: ValueAccuracy::Exact,
            ..
        } => 1,
        MetadataValue::Unknown => 2,
        MetadataValue::Unsupported => 3,
        MetadataValue::AccessDenied => 4,
        MetadataValue::Known { .. } => 5,
        _ => 2,
    }
}
fn value<T>(state: i64, v: Option<T>) -> Result<MetadataValue<T>> {
    Ok(match state {
        0 => MetadataValue::NotCollected,
        1 | 5 => MetadataValue::Known {
            value: v.ok_or(Error::Contract("missing property value"))?,
            accuracy: if state == 1 {
                ValueAccuracy::Exact
            } else {
                ValueAccuracy::LowerBound
            },
        },
        2 => MetadataValue::Unknown,
        3 => MetadataValue::Unsupported,
        4 => MetadataValue::AccessDenied,
        _ => return Err(Error::Contract("property state")),
    })
}
pub(super) fn instant(t: SystemTime) -> Option<(i64, u32)> {
    let ns = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos()).ok()?,
        Err(e) => -i128::try_from(e.duration().as_nanos()).ok()?,
    };
    Some((
        ns.div_euclid(1_000_000_000).try_into().ok()?,
        ns.rem_euclid(1_000_000_000) as u32,
    ))
}
pub(super) fn owner(v: NativeOwnerId) -> Vec<u8> {
    match v {
        NativeOwnerId::UnixUid(u) => {
            let mut b = vec![0];
            b.extend(u.to_be_bytes());
            b
        }
        #[cfg(windows)]
        NativeOwnerId::WindowsSid(s) => {
            let mut b = vec![1];
            b.extend(s.as_bytes());
            b
        }
        _ => vec![],
    }
}
fn decode_owner(b: &[u8]) -> Result<Option<NativeOwnerId>> {
    match b {
        [0, a, b, c, d] => Ok(Some(NativeOwnerId::UnixUid(u32::from_be_bytes([
            *a, *b, *c, *d,
        ])))),
        #[cfg(windows)]
        [1, raw @ ..] => Ok(Some(NativeOwnerId::WindowsSid(
            sortsize_core::WindowsSid::from_bytes(raw)
                .ok_or(Error::Contract("invalid owner SID"))?,
        ))),
        #[cfg(not(windows))]
        [1, ..] => Ok(None),
        _ => Err(Error::Contract("owner codec")),
    }
}
pub(super) const INSERT: &str = "INSERT INTO entry_properties VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(generation,id) DO UPDATE SET allocated_state=excluded.allocated_state,accessed_state=excluded.accessed_state,accessed_s=excluded.accessed_s,accessed_ns=excluded.accessed_ns,owner=excluded.owner";
pub(super) fn write(
    statement: &mut rusqlite::Statement<'_>,
    g: GenerationId,
    id: EntryId,
    p: Option<&NodeProperties>,
    allocated: Option<u64>,
) -> Result<()> {
    let default = NodeProperties {
        allocated: allocated.map_or(MetadataValue::NotCollected, |value| MetadataValue::Known {
            value,
            accuracy: ValueAccuracy::Exact,
        }),
        ..Default::default()
    };
    let p = p.unwrap_or(&default);
    let accessed = match p.accessed {
        MetadataValue::Known { value, .. } => {
            Some(instant(value).ok_or(Error::Contract("accessed range"))?)
        }
        _ => None,
    };
    let raw = match p.owner.id {
        MetadataValue::Known { value, .. } => Some(owner(value)),
        _ => None,
    };
    statement.execute(params![
        unsigned(g.0),
        unsigned(id.0),
        state(&p.allocated),
        state(&p.accessed),
        accessed.map(|t| t.0),
        accessed.map(|t| t.1),
        raw.map(|r| {
            let mut out = vec![state(&p.owner.id) as u8];
            out.extend(r);
            out
        })
        .unwrap_or_else(|| vec![state(&p.owner.id) as u8])
    ])?;
    // Allocated values remain in canonical entries, not duplicated in an auxiliary store.
    Ok(())
}
/// Shared decoding for point reads and bounded joined sort batches.
pub(super) fn decode(
    allocation: Option<u64>,
    a: i64,
    t: i64,
    s: Option<i64>,
    ns: Option<u32>,
    raw: &[u8],
) -> Result<NodeProperties> {
    let accessed = super::queries::time(s, ns);
    let state_id = *raw.first().ok_or(Error::Contract("empty owner"))? as i64;
    let own = if state_id == 1 || state_id == 5 {
        match decode_owner(&raw[1..])? {
            Some(v) => value(state_id, Some(v))?,
            None => MetadataValue::Unsupported,
        }
    } else {
        value(state_id, None)?
    };
    Ok(NodeProperties {
        allocated: value(a, allocation)?,
        accessed: value(t, accessed)?,
        owner: sortsize_core::EntryOwner { id: own },
    })
}
impl Store {
    /// Explicit additive migration, admitted under the existing disk budget.
    /// Read-only cache opening never calls this method.
    pub fn enable_properties(&mut self) -> Result<()> {
        self.enable_catalogue()?;
        if self
            .db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
            == 6
        {
            return Ok(());
        }
        let _disk = self.disk_admission(Self::WRITE_RESERVE)?;
        let tx = self.db.transaction()?;
        tx.execute_batch("CREATE TABLE entry_properties(generation BLOB NOT NULL,id BLOB NOT NULL,allocated_state INTEGER NOT NULL,accessed_state INTEGER NOT NULL,accessed_s INTEGER,accessed_ns INTEGER,owner BLOB NOT NULL,PRIMARY KEY(generation,id),FOREIGN KEY(generation,id) REFERENCES entries(generation,id),CHECK(allocated_state BETWEEN 0 AND 5),CHECK(accessed_state BETWEEN 0 AND 5),CHECK(length(owner) BETWEEN 1 AND 70),CHECK((accessed_s IS NULL)=(accessed_ns IS NULL)),CHECK(accessed_ns IS NULL OR accessed_ns BETWEEN 0 AND 999999999)) STRICT, WITHOUT ROWID;
CREATE TABLE collection_options(generation BLOB PRIMARY KEY REFERENCES generations(id),allocation INTEGER NOT NULL CHECK(allocation IN (0,1)),owners INTEGER NOT NULL CHECK(owners IN (0,1))) STRICT, WITHOUT ROWID;
CREATE TRIGGER immutable_properties_insert BEFORE INSERT ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_update BEFORE UPDATE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_properties_delete BEFORE DELETE ON entry_properties WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_insert BEFORE INSERT ON collection_options WHEN (SELECT sealed FROM generations WHERE id=new.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
CREATE TRIGGER immutable_options_update BEFORE UPDATE ON collection_options BEGIN SELECT RAISE(ABORT,'immutable collection options'); END;
CREATE TRIGGER immutable_options_delete BEFORE DELETE ON collection_options WHEN (SELECT sealed FROM generations WHERE id=old.generation)=1 BEGIN SELECT RAISE(ABORT,'sealed generation'); END;
PRAGMA user_version=6;")?;
        tx.commit()?;
        Ok(())
    }
    pub fn collection_options(&self, g: GenerationId) -> Result<Option<(bool, bool)>> {
        if self
            .db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
            < 6
        {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                "SELECT allocation,owners FROM collection_options WHERE generation=?1",
                [unsigned(g.0)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
    pub fn set_collection_options(
        &mut self,
        c: AcquisitionContext,
        allocation: bool,
        owners: bool,
    ) -> Result<()> {
        generation(&self.db, c)?;
        let _disk = self.disk_admission(64 * 1024)?;
        self.db.execute(
            "INSERT INTO collection_options VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
            params![unsigned(c.generation.0), allocation, owners],
        )?;
        if self.collection_options(c.generation)? != Some((allocation, owners)) {
            return Err(Error::Contract("immutable collection options"));
        }
        Ok(())
    }
    pub fn entry_properties(&self, key: EntryKey) -> Result<NodeProperties> {
        if key.inventory != self.inventory {
            return Err(Error::Contract("foreign metadata key"));
        }
        let allocation: Option<u64> = self.db.query_row(
            "SELECT allocated FROM entries WHERE generation=?1 AND id=?2",
            params![unsigned(key.generation.0), unsigned(key.entry.0)],
            |r| opt_number(r, 0),
        )?;
        if self
            .db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?
            < 6
        {
            return Ok(NodeProperties {
                allocated: allocation.map_or(MetadataValue::NotCollected, |value| {
                    MetadataValue::Known {
                        value,
                        accuracy: ValueAccuracy::Exact,
                    }
                }),
                ..Default::default()
            });
        }
        let row=self.db.query_row("SELECT allocated_state,accessed_state,accessed_s,accessed_ns,owner FROM entry_properties WHERE generation=?1 AND id=?2",params![unsigned(key.generation.0),unsigned(key.entry.0)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,Option<u32>>(3)?,r.get::<_,Vec<u8>>(4)?))).optional()?;
        let Some((a, t, s, ns, raw)) = row else {
            return Ok(NodeProperties::default());
        };
        decode(allocation, a, t, s, ns, &raw)
    }

    pub fn entry_modified(&self, key: EntryKey) -> Result<MetadataValue<SystemTime>> {
        if key.inventory != self.inventory {
            return Err(Error::Contract("foreign metadata key"));
        }
        let (s,n,known):(Option<i64>,Option<u32>,bool)=self.db.query_row("SELECT modified_s,modified_ns,metadata_known FROM entries WHERE generation=?1 AND id=?2",params![unsigned(key.generation.0),unsigned(key.entry.0)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(super::queries::time(s, n).map_or(
            if known {
                MetadataValue::NotCollected
            } else {
                MetadataValue::Unknown
            },
            |value| MetadataValue::Known {
                value,
                accuracy: ValueAccuracy::Exact,
            },
        ))
    }
}
