//! Bounded, cooperative directory ordering, independent of the details page.
use super::*;
use sortsize_store::direct_tree::query::{Cursor, PageQuery};

pub(super) struct BranchQuery {
    job: PageQuery,
    retained: Option<PageQuery>,
    anchor: Option<Cursor>,
    previous: Vec<(Option<Cursor>, Option<EntryId>)>,
    reveal: Option<EntryId>,
}
impl BranchQuery {
    fn new(tree: &DirectTree, parent: EntryId) -> Result<Self> {
        Ok(Self {
            job: Self::job(tree, parent, None, None)?,
            retained: None,
            anchor: None,
            previous: Vec::new(),
            reveal: None,
        })
    }
    fn job(
        tree: &DirectTree,
        parent: EntryId,
        anchor: Option<Cursor>,
        reveal: Option<EntryId>,
    ) -> Result<PageQuery> {
        let job = PageQuery::new(tree, Some(parent), None, 0, anchor, BRANCH_LIMIT)?.folders_only();
        if let Some(entry) = reveal {
            job.start_at(tree, entry)
        } else {
            Ok(job)
        }
    }
    fn reset(&mut self, tree: &DirectTree, parent: EntryId) -> Result<()> {
        let next = Self::job(tree, parent, self.anchor.clone(), self.reveal)?;
        let old = std::mem::replace(&mut self.job, next);
        if old.done() {
            self.retained = Some(old);
        }
        Ok(())
    }
}
impl DirectActive {
    pub(super) fn branch_turn(
        &mut self,
        tree: &DirectTree,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.branch_queries
            .retain(|id, _| self.expanded.contains_key(id));
        let started = Instant::now();
        let mut parents = self.expanded.keys().copied().collect::<Vec<_>>();
        let split =
            parents.partition_point(|id| self.branch_cursor.is_some_and(|last| *id <= last));
        parents.rotate_left(split);
        for parent in parents {
            self.branch_cursor = Some(parent);
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.branch_queries.entry(parent)
            {
                entry.insert(BranchQuery::new(tree, parent)?);
            }
            let query = self.branch_queries.get_mut(&parent).unwrap();
            if query.job.done() && query.job.revision != tree.revision() {
                query.reset(tree, parent)?;
            }
            let refreshing = query.retained.is_some();
            if !query.job.done() && query.job.advance(tree, cancel)? {
                query.retained = None;
                // A refresh of a branch already visible can join the next
                // periodic publication; explicit expansion still publishes
                // as soon as its first order is available.
                self.force |= !refreshing;
            }
            if started.elapsed() >= Duration::from_millis(8) {
                break;
            }
        }
        Ok(())
    }
    pub(super) fn page_branch(
        &mut self,
        tree: &DirectTree,
        parent: EntryId,
        forward: bool,
    ) -> Result<()> {
        let Some(query) = self.branch_queries.get_mut(&parent) else {
            return Ok(());
        };
        if !query.job.done() {
            return Ok(());
        }
        if forward {
            let Some(next) = query.job.next() else {
                return Ok(());
            };
            if query.previous.len() == 128 {
                query.previous.remove(0);
            }
            query
                .previous
                .push((query.anchor.take(), query.reveal.take()));
            query.anchor = Some(next);
        } else if let Some(previous) = query.previous.pop() {
            (query.anchor, query.reveal) = previous;
        } else {
            return Ok(());
        }
        query.reset(tree, parent)
    }
    pub(super) fn reveal_branch(
        &mut self,
        tree: &DirectTree,
        parent: EntryId,
        child: EntryId,
    ) -> Result<()> {
        if let Some(query) = self.branch_queries.get(&parent)
            && query.job.done()
            && query.job.rows(tree)?.iter().any(|row| row.id == child)
        {
            return Ok(());
        }
        let mut query = BranchQuery::new(tree, parent)?;
        query.reveal = Some(child);
        query.previous.push((None, None));
        query.reset(tree, parent)?;
        self.branch_queries.insert(parent, query);
        Ok(())
    }
    pub(super) fn branch_page(
        &self,
        tree: &DirectTree,
        parent: EntryId,
    ) -> Result<(DirectPage, bool)> {
        if let Some(query) = self.branch_queries.get(&parent)
            && query.job.done()
        {
            return Ok((
                DirectPage {
                    rows: query.job.rows(tree)?,
                    more: query.job.more(),
                    next: None,
                },
                !query.previous.is_empty(),
            ));
        }
        if let Some(retained) = self
            .branch_queries
            .get(&parent)
            .and_then(|q| q.retained.as_ref())
        {
            return Ok((
                DirectPage {
                    rows: retained.rows(tree)?,
                    more: false,
                    next: None,
                },
                false,
            ));
        }
        if let Some(entry) = self.branch_queries.get(&parent).and_then(|q| q.reveal) {
            return Ok((
                DirectPage {
                    rows: vec![tree.row(entry)?],
                    more: false,
                    next: None,
                },
                false,
            ));
        }
        // Keep a bounded observed window available while the global order is
        // prepared; the common status explicitly identifies the pending order.
        let mut page = tree.children(parent, None, BRANCH_LIMIT, true)?;
        page.rows
            .sort_by(|a, b| b.logical.cmp(&a.logical).then_with(|| a.path.cmp(&b.path)));
        page.more = false;
        Ok((page, false))
    }
    pub(super) fn branches_pending(&self, revision: u64) -> bool {
        self.expanded.keys().any(|id| {
            self.branch_queries
                .get(id)
                .is_none_or(|q| !q.job.done() || q.job.revision != revision)
        })
    }
}
