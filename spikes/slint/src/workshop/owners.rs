//! Reuse the bounded owner coordinator. No lookup before explicit UI consent.
use super::*;
use sortsize_core::{
    MetadataValue,
    resources::{Category, Governor, Reservation},
};
use sortsize_owner_resolution::{
    LookupLimits, OwnerLookup, OwnerNameResolution, SystemOwnerLookup, SystemProviderConsent,
};
struct BudgetedLookup {
    inner: SystemOwnerLookup,
    _memory: Arc<Reservation>,
}
impl OwnerLookup for BudgetedLookup {
    fn lookup(
        &self,
        id: sortsize_core::NativeOwnerId,
        limits: LookupLimits,
    ) -> OwnerNameResolution {
        self.inner.lookup(id, limits)
    }
}
impl Host {
    pub(super) fn poll_owners(&mut self, w: &SortSizeWindow) {
        #[cfg(any(windows, target_os = "linux"))]
        if w.get_targeted_owner() {
            self.poll_windows_owners(w);
            return;
        }
        let enabled = w.get_resolve_owner();
        let Some(current) = self.current.as_ref().filter(|p| p.epoch == self.epoch) else {
            return;
        };
        let request = (self.epoch, current.token, enabled);
        if self.owner_request != Some(request) {
            if enabled && self.owner_task.is_none() {
                let Some(memory) =
                    Governor::global().reserve(Category::Queries, 2 * 1024 * 1024, None)
                else {
                    w.set_owner_status(
                        crate::presentation::OwnerStatus::Unavailable
                            .render()
                            .into(),
                    );
                    return;
                };
                let provider = Arc::new(BudgetedLookup {
                    inner: SystemOwnerLookup::new(
                        SystemProviderConsent::AllowPotentialNetworkLookup,
                    ),
                    _memory: Arc::new(memory),
                });
                match crate::owner_view::Task::start(provider) {
                    Ok(task) => self.owner_task = Some(task),
                    Err(_) => {
                        w.set_owner_status(
                            crate::presentation::OwnerStatus::Unavailable
                                .render()
                                .into(),
                        );
                        return;
                    }
                }
            }
            if let Some(task) = self.owner_task.as_mut() {
                let owners = current.owners.clone().into_boxed_slice();
                let visible = owners
                    .iter()
                    .filter_map(|p| match p.id {
                        MetadataValue::Known { value, .. } => Some(value),
                        _ => None,
                    })
                    .collect();
                let n = owners.len();
                let data = crate::owner_view::OwnerData {
                    owners,
                    ranges: vec![0..0; n].into_boxed_slice(),
                    children: Vec::new().into_boxed_slice(),
                    directories: current
                        .frame
                        .rows
                        .iter()
                        .map(|r| r.kind == sortsize_store::inventory::Kind::Directory)
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                };
                task.request(crate::owner_view::Request {
                    generation: self.epoch,
                    page: current.token as u64,
                    enabled,
                    data: Some(Arc::new(data)),
                    visible,
                    sort: None,
                });
            }
            self.owner_request = Some(request);
            if enabled {
                w.set_owner_status(crate::presentation::OwnerStatus::Resolving.render().into());
            }
            if !enabled {
                self.owner_names = Arc::default();
                self.owner_names_tag = None;
                let mut rows = current.details.clone();
                for (r, o) in rows.iter_mut().zip(&current.owners) {
                    r.owner = crate::owner_view::fallback(o.id);
                }
                w.set_details(Rc::new(VecModel::from(rows)).into());
                w.set_owner_status(crate::presentation::OwnerStatus::Idle.render().into());
            }
        }
        let Some(task) = self.owner_task.as_mut() else {
            return;
        };
        if let Some(p) = task
            .poll()
            .page
            .filter(|p| enabled && p.generation == self.epoch && p.page == current.token as u64)
        {
            self.owner_names = p.names.clone();
            self.owner_names_tag = Some((self.epoch, current.token));
            let mut rows = current.details.clone();
            for (r, o) in rows.iter_mut().zip(&current.owners) {
                if let MetadataValue::Known { value, .. } = o.id {
                    r.owner = crate::owner_view::label_for(&p.names, value)
                        .cloned()
                        .unwrap_or_else(|| crate::owner_view::fallback(o.id));
                }
            }
            w.set_details(Rc::new(VecModel::from(rows)).into());
            w.set_owner_status(
                crate::presentation::OwnerStatus::Page {
                    pending: p.pending,
                    waiting: p.waiting_for_previous,
                }
                .render()
                .into(),
            );
        }
    }
}
