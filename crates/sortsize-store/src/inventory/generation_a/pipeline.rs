//! Prospective admission for the complete reader/writer/publication pipeline.
use super::*;

// A planned consultation includes the derived folder connection. Its existing
// cache rebalancing lends 1 MiB from this lease at >=2 MiB. Do not reserve the
// whole SQLite category for workers and require an extra, unavailable lease
// only after acquisition starts. Global/category limits remain unchanged.
const READER_MIN: u64 = 2 * FRAME;
const READER_PREFERRED: u64 = READER_MIN + FRAME / 2;

/// All three slots are charged before the first cache opens. Slots are moved
/// into connection owners, never represented by an uncounted cache allowance.
pub struct Pipeline {
    reader: Option<Reservation>,
    writer: Option<Reservation>,
    publication: Option<Reservation>,
}
#[derive(Debug, Clone, Copy)]
pub struct PlannedBytes {
    pub reader: u64,
    pub writer: u64,
    pub publication: u64,
}
impl Pipeline {
    pub fn plan() -> Result<Self> {
        let governor = Governor::global();
        governor.refresh_native();
        Self::plan_in(&governor)
    }
    pub fn for_publication() -> Result<Self> {
        let governor = Governor::global();
        governor.refresh_native();
        Self::publication_in(&governor)
    }
    pub fn publication_in(governor: &Governor) -> Result<Self> {
        let usage = governor.usage();
        let used = usage
            .categories
            .iter()
            .try_fold(0u64, |n, v| n.checked_add(*v))
            .ok_or(Error::Contract("pipeline memory accounting overflow"))?;
        let available = usage.limits.bytes.saturating_sub(used).min(
            usage.limits.categories[Category::Sqlite as usize]
                .saturating_sub(usage.categories[Category::Sqlite as usize]),
        );
        if available < READER_MIN + FRAME {
            return Err(Error::Contract(
                "publication pipeline requires reader and builder capacity",
            ));
        }
        let reader_bytes = (available - FRAME).min(READER_PREFERRED);
        let reader = governor
            .reserve(Category::Sqlite, reader_bytes, None)
            .ok_or(Error::Contract("publication reader admission changed"))?;
        let publication = governor
            .reserve(
                Category::Sqlite,
                (available - reader_bytes).min(8 * FRAME),
                None,
            )
            .ok_or(Error::Contract("publication builder admission changed"))?;
        Ok(Self {
            reader: Some(reader),
            writer: None,
            publication: Some(publication),
        })
    }
    pub fn plan_in(governor: &Governor) -> Result<Self> {
        let usage = governor.usage();
        let used = usage
            .categories
            .iter()
            .try_fold(0u64, |n, v| n.checked_add(*v))
            .ok_or(Error::Contract("pipeline memory accounting overflow"))?;
        let available = usage.limits.bytes.saturating_sub(used).min(
            usage.limits.categories[Category::Sqlite as usize]
                .saturating_sub(usage.categories[Category::Sqlite as usize]),
        );
        if available < READER_MIN + 2 * FRAME {
            return Err(Error::Contract(
                "pipeline requires reader, writer and publication capacity",
            ));
        }
        let reader_bytes = (available - 2 * FRAME).min(READER_PREFERRED);
        let worker_bytes = ((available - reader_bytes) / 2).min(8 * FRAME);
        let slot = |bytes| {
            governor
                .reserve(Category::Sqlite, bytes, None)
                .ok_or(Error::Contract("pipeline prospective admission changed"))
        };
        let reader = slot(reader_bytes)?;
        let writer = slot(worker_bytes)?;
        let publication = slot(worker_bytes)?;
        Ok(Self {
            reader: Some(reader),
            writer: Some(writer),
            publication: Some(publication),
        })
    }

    /// Borrow the prospectively admitted reader slot for an owned live snapshot.
    /// This is a dated acquisition preview, never a certified published reader.
    pub fn live_preview(
        &mut self,
        source: &Store,
        context: AcquisitionContext,
    ) -> Result<crate::inventory::live_preview::LivePreview> {
        self.preview_snapshot(source, context, false)
    }
    fn preview_snapshot(
        &mut self,
        source: &Store,
        context: AcquisitionContext,
        sealed: bool,
    ) -> Result<crate::inventory::live_preview::LivePreview> {
        let memory = self.reader()?;
        match crate::inventory::live_preview::LivePreview::open(source, context, memory, sealed) {
            Ok(preview) => Ok(preview),
            Err(failure) => {
                let (error, memory) = *failure;
                self.reader = Some(memory);
                Err(error)
            }
        }
    }
    /// Transfer the reader slot to an exact sealed session snapshot. This
    /// read-only connection shares ownership of the ephemeral candidate but
    /// carries no published-generation certificate.
    pub fn sealed_fast_reader(
        &mut self,
        source: &Store,
        context: AcquisitionContext,
    ) -> Result<Store> {
        if !source.is_fast_candidate() || source.latest_sealed()? != Some(context.generation) {
            return Err(Error::Contract(
                "fast reader requires a sealed ephemeral candidate",
            ));
        }
        Ok(self.preview_snapshot(source, context, true)?.into_store())
    }

    /// Close all preview connections before returning their reader admission.
    pub fn restore_preview(
        &mut self,
        preview: crate::inventory::live_preview::LivePreview,
    ) -> Result<()> {
        if self.reader.is_some() {
            return Err(Error::Contract("pipeline preview slot already restored"));
        }
        self.reader = Some(preview.close()?);
        Ok(())
    }
    pub fn remaining(&self) -> PlannedBytes {
        PlannedBytes {
            reader: self.reader.as_ref().map_or(0, Reservation::bytes),
            writer: self.writer.as_ref().map_or(0, Reservation::bytes),
            publication: self.publication.as_ref().map_or(0, Reservation::bytes),
        }
    }
    pub(super) fn reader(&mut self) -> Result<Reservation> {
        self.reader
            .take()
            .ok_or(Error::Contract("pipeline reader slot already owned"))
    }
    /// Admit both owners before moving either slot. Failure must not consume
    /// the builder when a live preview still owns the prospective reader.
    pub(super) fn publication_and_reader(&mut self) -> Result<(Reservation, Reservation)> {
        if self.publication.is_none() || self.reader.is_none() {
            return Err(Error::Contract(
                "publication and reader slots must both be available",
            ));
        }
        Ok((self.publication()?, self.reader()?))
    }
    pub(super) fn writer(&mut self) -> Result<Reservation> {
        self.writer
            .take()
            .ok_or(Error::Contract("pipeline writer slot already owned"))
    }
    /// A retry may inspect its interrupted DB with the already admitted builder
    /// slot; it returns that owner only after the inspection connection closes.
    pub(super) fn restore_publication(&mut self, memory: Reservation) -> Result<()> {
        if self.publication.is_some() {
            return Err(Error::Contract("pipeline publication slot already present"));
        }
        self.publication = Some(memory);
        Ok(())
    }
    pub(super) fn publication(&mut self) -> Result<Reservation> {
        self.publication
            .take()
            .ok_or(Error::Contract("pipeline publication slot already owned"))
    }
}
