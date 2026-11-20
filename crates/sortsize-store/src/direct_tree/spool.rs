use crate::{
    codec::NativeName,
    direct_save::ReplayBatch,
    inventory::{Error, Kind, Observation, Result, differential::NativeEvidence},
};
use sortsize_core::{
    EntryOwner, LocalFileId, MetadataValue, NativeOwnerId, ValueAccuracy,
    acquisition::{NodeProperties, Stamp},
    inventory_contract::{EntryId, ListingEnd},
};
use std::{
    io::{self, Read},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_BATCH: usize = 128 * 1024;
pub(crate) const MAX_PAGE_ROWS: usize = 256;
pub(crate) const MAX_PAGE_BYTES: usize = 384 * 1024;

pub(crate) fn invalid() -> Error {
    Error::Contract("invalid direct analysis spool")
}

pub(crate) struct Encoder(pub Vec<u8>);

impl Encoder {
    fn byte(&mut self, value: u8) {
        self.0.push(value);
    }
    fn raw(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }
    pub(crate) fn word(&mut self, value: usize) -> Result<()> {
        self.raw(&u16::try_from(value).map_err(|_| invalid())?.to_le_bytes());
        Ok(())
    }
    fn u32(&mut self, value: u32) {
        self.raw(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.raw(&value.to_le_bytes());
    }
    fn timestamp(&mut self, (seconds, nanos): (i64, u32)) {
        self.raw(&seconds.to_le_bytes());
        self.u32(nanos);
    }
    fn option<T>(
        &mut self,
        value: Option<T>,
        write: impl FnOnce(&mut Self, T) -> Result<()>,
    ) -> Result<()> {
        self.byte(u8::from(value.is_some()));
        if let Some(value) = value {
            write(self, value)?;
        }
        Ok(())
    }
    fn size(&mut self, value: Option<u64>) -> Result<()> {
        self.option(value, |e, value| {
            e.u64(value);
            Ok(())
        })
    }
    fn time(&mut self, time: SystemTime) {
        let (before, duration) = match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => (false, duration),
            Err(error) => (true, error.duration()),
        };
        self.byte(u8::from(before));
        self.u64(duration.as_secs());
        self.u32(duration.subsec_nanos());
    }
    fn identity(&mut self, identity: Option<LocalFileId>) -> Result<()> {
        match identity {
            None => self.byte(0),
            Some(LocalFileId::Unix { device, inode }) => {
                self.byte(1);
                self.u64(device);
                self.u64(inode);
            }
            Some(LocalFileId::Windows {
                volume_serial,
                file_id,
            }) => {
                self.byte(2);
                self.u64(volume_serial);
                self.raw(&file_id);
            }
            Some(LocalFileId::WindowsLegacy { volume, index }) => {
                self.byte(3);
                self.u32(volume);
                self.u64(index);
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    fn metadata<T: Copy>(
        &mut self,
        value: MetadataValue<T>,
        write: impl FnOnce(&mut Self, T) -> Result<()>,
    ) -> Result<()> {
        match value {
            MetadataValue::NotCollected => self.byte(0),
            MetadataValue::Unknown => self.byte(1),
            MetadataValue::Unsupported => self.byte(2),
            MetadataValue::AccessDenied => self.byte(3),
            MetadataValue::Known { value, accuracy } => {
                self.byte(match accuracy {
                    ValueAccuracy::Exact => 4,
                    ValueAccuracy::LowerBound => 5,
                    _ => return Err(invalid()),
                });
                write(self, value)?;
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    fn properties(&mut self, properties: &NodeProperties) -> Result<()> {
        self.metadata(properties.allocated, |e, value| {
            e.u64(value);
            Ok(())
        })?;
        self.metadata(properties.accessed, |e, time| {
            e.time(time);
            Ok(())
        })?;
        self.metadata(properties.owner.id, |e, id| {
            match id {
                NativeOwnerId::UnixUid(uid) => {
                    e.byte(0);
                    e.u32(uid);
                }
                #[cfg(windows)]
                NativeOwnerId::WindowsSid(sid) => {
                    e.byte(1);
                    e.byte(sid.as_bytes().len() as u8);
                    e.raw(sid.as_bytes());
                }
                _ => return Err(invalid()),
            }
            Ok(())
        })
    }
    pub(crate) fn header(
        &mut self,
        parent: EntryId,
        sequence: u64,
        end: Option<ListingEnd>,
        directory: Option<&Stamp>,
        count: usize,
    ) -> Result<()> {
        self.u32(u32::try_from(parent.0).map_err(|_| invalid())?);
        self.u64(sequence);
        self.byte(match end {
            None => 0,
            Some(ListingEnd::Exhausted) => 1,
            Some(ListingEnd::Incomplete) => 2,
        });
        self.option(directory, |e, stamp| {
            e.identity(stamp.identity)?;
            for time in [stamp.created, stamp.modified] {
                e.option(time, |e, time| {
                    e.time(time);
                    Ok(())
                })?;
            }
            e.u64(stamp.length);
            e.byte(u8::from(stamp.directory));
            e.byte(u8::from(stamp.regular));
            e.properties(&stamp.properties)
        })?;
        self.word(count)
    }
    pub(crate) fn record(
        &mut self,
        id: u32,
        name: &[u8],
        observation: &Observation,
        evidence: Option<&NativeEvidence>,
        properties: &NodeProperties,
    ) -> Result<()> {
        let length_at = self.0.len();
        self.word(0)?;
        self.word(name.len())?;
        self.raw(name);
        self.u32(id);
        self.byte(observation.kind as u8);
        self.byte(u8::from(observation.metadata_known));
        self.size(observation.logical)?;
        self.size(observation.allocated)?;
        self.option(observation.modified, |e, date| {
            e.timestamp(date);
            Ok(())
        })?;
        self.option(evidence, |e, proof| {
            e.identity(proof.identity)?;
            e.option(proof.created, |e, date| {
                e.timestamp(date);
                Ok(())
            })
        })?;
        self.properties(properties)?;
        let length = u16::try_from(self.0.len() - length_at - 2).map_err(|_| invalid())?;
        self.0[length_at..length_at + 2].copy_from_slice(&length.to_le_bytes());
        Ok(())
    }
}

struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let (bytes, remaining) = self.0.split_at_checked(length).ok_or_else(invalid)?;
        self.0 = remaining;
        Ok(bytes)
    }
    fn number<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.number::<1>()?[0])
    }
    fn word(&mut self) -> Result<usize> {
        Ok(u16::from_le_bytes(self.number()?).into())
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.number()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.number()?))
    }
    fn boolean(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid()),
        }
    }
    fn timestamp(&mut self) -> Result<(i64, u32)> {
        let seconds = i64::from_le_bytes(self.number()?);
        let nanos = self.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(invalid());
        }
        Ok((seconds, nanos))
    }
    fn option<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.boolean()? {
            Ok(Some(read(self)?))
        } else {
            Ok(None)
        }
    }
    fn time(&mut self) -> Result<SystemTime> {
        let before = self.boolean()?;
        let seconds = self.u64()?;
        let nanos = self.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(invalid());
        }
        let duration = Duration::new(seconds, nanos);
        if before {
            UNIX_EPOCH.checked_sub(duration)
        } else {
            UNIX_EPOCH.checked_add(duration)
        }
        .ok_or_else(invalid)
    }
    fn identity(&mut self) -> Result<Option<LocalFileId>> {
        Ok(match self.byte()? {
            0 => None,
            1 => Some(LocalFileId::Unix {
                device: self.u64()?,
                inode: self.u64()?,
            }),
            2 => Some(LocalFileId::Windows {
                volume_serial: self.u64()?,
                file_id: self.number()?,
            }),
            3 => Some(LocalFileId::WindowsLegacy {
                volume: self.u32()?,
                index: self.u64()?,
            }),
            _ => return Err(invalid()),
        })
    }
    fn metadata<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<MetadataValue<T>> {
        Ok(match self.byte()? {
            0 => MetadataValue::NotCollected,
            1 => MetadataValue::Unknown,
            2 => MetadataValue::Unsupported,
            3 => MetadataValue::AccessDenied,
            state @ (4 | 5) => MetadataValue::Known {
                value: read(self)?,
                accuracy: if state == 4 {
                    ValueAccuracy::Exact
                } else {
                    ValueAccuracy::LowerBound
                },
            },
            _ => return Err(invalid()),
        })
    }
    fn properties(&mut self) -> Result<NodeProperties> {
        Ok(NodeProperties {
            allocated: self.metadata(Self::u64)?,
            accessed: self.metadata(Self::time)?,
            owner: EntryOwner {
                id: self.metadata(|d| {
                    Ok(match d.byte()? {
                        0 => NativeOwnerId::UnixUid(d.u32()?),
                        #[cfg(windows)]
                        1 => {
                            let length = d.byte()?.into();
                            let sid = sortsize_core::WindowsSid::from_bytes(d.take(length)?)
                                .ok_or_else(invalid)?;
                            NativeOwnerId::WindowsSid(sid)
                        }
                        _ => return Err(invalid()),
                    })
                })?,
            },
        })
    }
    fn stamp(&mut self) -> Result<Stamp> {
        Ok(Stamp {
            identity: self.identity()?,
            created: self.option(Self::time)?,
            modified: self.option(Self::time)?,
            length: self.u64()?,
            directory: self.boolean()?,
            regular: self.boolean()?,
            properties: self.properties()?,
        })
    }
}

pub(crate) fn encoded_name(bytes: &[u8]) -> Result<&[u8]> {
    let mut record = Decoder(bytes);
    let name_len = record.word()?;
    record.take(name_len)
}

pub(crate) struct DecodedRecord {
    pub id: EntryId,
    pub observation: Observation,
    pub evidence: Option<NativeEvidence>,
    pub properties: NodeProperties,
}

pub(crate) fn record(bytes: &[u8]) -> Result<DecodedRecord> {
    let mut d = Decoder(bytes);
    let name_len = d.word()?;
    let name = NativeName::decode(d.take(name_len)?).map_err(|_| invalid())?;
    let id = EntryId(d.u32()?.into());
    let kind = match d.byte()? {
        0 => Kind::Unknown,
        1 => Kind::File,
        2 => Kind::Directory,
        3 => Kind::Symlink,
        4 => Kind::Special,
        _ => return Err(invalid()),
    };
    let observation = Observation {
        name,
        kind,
        metadata_known: d.boolean()?,
        logical: d.option(Decoder::u64)?,
        allocated: d.option(Decoder::u64)?,
        modified: d.option(Decoder::timestamp)?,
    };
    let evidence = d.option(|d| {
        Ok(NativeEvidence {
            identity: d.identity()?,
            created: d.option(Decoder::timestamp)?,
        })
    })?;
    let properties = d.properties()?;
    if !d.0.is_empty() {
        return Err(invalid());
    }
    Ok(DecodedRecord {
        id,
        observation,
        evidence,
        properties,
    })
}

pub(crate) fn batch(bytes: &[u8]) -> Result<ReplayBatch> {
    let mut d = Decoder(bytes);
    let parent = EntryId(d.u32()?.into());
    let sequence = d.u64()?;
    let end = match d.byte()? {
        0 => None,
        1 => Some(ListingEnd::Exhausted),
        2 => Some(ListingEnd::Incomplete),
        _ => return Err(invalid()),
    };
    let directory = d.option(Decoder::stamp)?;
    let count = d.word()?;
    if count > 512 {
        return Err(invalid());
    }
    let mut observations = Vec::with_capacity(count);
    let mut evidence = Vec::with_capacity(count);
    let mut properties = Vec::with_capacity(count);
    let mut children = Vec::new();
    for _ in 0..count {
        let length = d.word()?;
        let entry = record(d.take(length)?)?;
        if entry.observation.kind == Kind::Directory {
            children.push((entry.id, entry.observation.name.clone()));
        }
        observations.push(entry.observation);
        evidence.push(entry.evidence);
        properties.push(entry.properties);
    }
    if !d.0.is_empty() {
        return Err(invalid());
    }
    Ok(ReplayBatch {
        parent,
        sequence,
        end,
        observations,
        evidence,
        properties,
        directory,
        children,
    })
}

pub(crate) fn read_record(reader: &mut impl Read, scratch: &mut Vec<u8>) -> io::Result<()> {
    let mut prefix = [0; 2];
    reader.read_exact(&mut prefix)?;
    scratch.resize(usize::from(u16::from_le_bytes(prefix)), 0);
    reader.read_exact(scratch)
}
