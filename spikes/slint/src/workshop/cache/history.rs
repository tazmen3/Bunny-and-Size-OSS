//! Presentation of retained measurements. Sizes are logical bytes per path.
use super::*;
use crate::{HistoryPoint, HistoryRow, units::Units};
use sortsize_store::inventory::{
    Result,
    differential::{RootChange, largest_root_entries},
};

#[derive(Default)]
pub(super) struct Report {
    pub heading: String,
    pub scope: String,
    pub rows: Vec<HistoryRow>,
    pub timeline: Option<Timeline>,
}

pub(super) struct Timeline {
    pub points: Vec<HistoryPoint>,
    pub zero: String,
    pub middle: String,
    pub maximum: String,
    pub first: String,
    pub last: String,
}

pub(super) fn retained(saved: &Saved) -> bool {
    !is_snapshot(saved) && saved.measured.is_some()
}

pub(super) fn coverage(coverage: ScopeCoverage) -> &'static str {
    if coverage == ScopeCoverage::Complete {
        tr!("Complete")
    } else {
        tr!("Partial")
    }
}

pub(super) fn size(units: Units, logical: Option<u64>) -> String {
    logical.map_or_else(|| tr!("Unavailable").into(), |n| units.format_file(n))
}

pub(super) fn delta(units: Units, n: i128) -> String {
    let sign = match n.cmp(&0) {
        std::cmp::Ordering::Less => "−",
        std::cmp::Ordering::Equal => "",
        std::cmp::Ordering::Greater => "+",
    };
    format!("{sign}{}", units.format_file(n.unsigned_abs() as u64))
}

fn entry(
    row: impl Into<slint::SharedString>,
    type_label: &str,
    value: String,
    direction: i32,
) -> HistoryRow {
    HistoryRow {
        title: row.into(),
        state: type_label.into(),
        value: value.into(),
        direction,
    }
}

pub(super) fn open_selected(request: &Request, saved: &Saved) -> Result<service::Active> {
    let active = service::Active::open_saved(request.epoch, saved, None, &request.cancel)?;
    let actual = active.session.store().saved(saved.context.generation)?;
    if actual.context != saved.context
        || actual.root != saved.root
        || actual.measured != saved.measured
    {
        return Err(sortsize_store::inventory::Error::Contract(
            "saved measure changed; refresh the catalogue",
        ));
    }
    Ok(active)
}

pub(super) fn ranking(request: &Request, saved: &Saved, units: Units) -> Result<Report> {
    let scratch = service::work_location()?.temporary("bunny-ranking-")?;
    let source = open_selected(request, saved)?;
    let store = source.session.store();
    let lease = store.lease(saved.context.generation)?;
    let measurement = &lease.measurement;
    let rows = largest_root_entries(&lease, scratch.path(), &request.cancel)?
        .into_iter()
        .map(|r| {
            entry(
                label(Path::new(&r.name)),
                if r.is_folder {
                    tr!("Folder")
                } else {
                    tr!("File")
                },
                units.format_file(r.logical),
                0,
            )
        })
        .collect::<Vec<_>>();
    Ok(Report {
        heading: tr!("Largest entries · observed logical size").into(),
        scope: format!(
            "{}\n{} · {} · {}\n{}",
            label(&measurement.root),
            date(measurement.measured),
            coverage(measurement.coverage),
            size(units, measurement.logical),
            if rows.is_empty() {
                tr!("No entries with a known logical size.")
            } else {
                tr!("Direct files and immediate folders, including their descendants.")
            }
        ),
        rows,
        timeline: None,
    })
}

pub(super) fn contributions(diff: &Diff<'_>, changes: &[RootChange], units: Units) -> Report {
    let (old, new) = diff.measurements();
    let rows = changes
        .iter()
        .map(|r| {
            entry(
                label(Path::new(&r.name)),
                "",
                delta(units, r.delta),
                r.delta.signum() as i32,
            )
        })
        .collect::<Vec<_>>();
    let note = if !diff.summary().compatible {
        tr!("The two scopes could not be matched.")
    } else if diff.summary().comparable_delta.is_none() {
        tr!("The comparison needs two complete measures with known logical totals.")
    } else if rows.is_empty() {
        tr!("No logical size changes in immediate folders.")
    } else {
        tr!(
            "Up to five increases and five decreases among immediate folders. Direct files also affect the total."
        )
    };
    let scope = if old.root == new.root {
        label(&old.root)
    } else {
        format!("{} → {}", label(&old.root), label(&new.root))
    };
    Report {
        heading: tr!("Folder changes · logical size").into(),
        scope: format!("{scope}\n{note}"),
        rows,
        timeline: None,
    }
}

/// Keep every retained acquisition at this native path in date order. A different
/// scope or incomplete acquisition occupies a position but breaks the line.
pub(super) fn evolution(selected: &Saved, catalogue: &[Saved], units: Units) -> Report {
    let mut seen = std::collections::HashSet::new();
    let mut measures = catalogue
        .iter()
        .filter(|s| !is_snapshot(s) && s.root == selected.root && seen.insert(s.context))
        .collect::<Vec<_>>();
    measures.sort_by_key(|s| (s.measured.unwrap_or(s.started), s.started));
    let logical = |s: &Saved| {
        if selected.same_scope(s)
            && s.measured.is_some()
            && s.coverage == ScopeCoverage::Complete
            && s.unknown == 0
        {
            s.logical
        } else {
            None
        }
    };
    let mut report = Report {
        heading: tr!("Logical size over time").into(),
        scope: label(&selected.root),
        ..Default::default()
    };
    let count = measures.iter().filter_map(|s| logical(s)).count();
    let axis_max = measures
        .iter()
        .filter_map(|s| logical(s))
        .max()
        .unwrap_or(0)
        .max(2);
    let first = measures
        .first()
        .map_or(selected.started, |s| s.measured.unwrap_or(s.started));
    let last = measures
        .last()
        .map_or(first, |s| s.measured.unwrap_or(s.started));
    let range = i128::from(last) - i128::from(first);
    let mut previous = None;
    let mut points = Vec::with_capacity(measures.len());
    for s in measures {
        let time = s.measured.unwrap_or(s.started);
        let value = logical(s);
        let state = if s.coverage != ScopeCoverage::Complete || s.measured.is_none() {
            tr!("Partial")
        } else if !selected.same_scope(s) {
            tr!("Incomparable scope")
        } else if value.is_none() {
            tr!("Unavailable")
        } else {
            tr!("Complete")
        };
        report
            .rows
            .push(entry(date(time), state, size(units, s.logical), 0));
        let x = if range == 0 {
            0.5
        } else {
            ((i128::from(time) - i128::from(first)) as f64 / range as f64) as f32
        };
        let y = value.map_or(1., |n| (1. - n as f64 / axis_max as f64) as f32);
        let from = previous.filter(|(timestamp, _, _)| *timestamp < time);
        let (_, previous_x, previous_y) = from.unwrap_or((time, x, y));
        points.push(HistoryPoint {
            x,
            y,
            previous_x,
            previous_y,
            connect: value.is_some() && from.is_some(),
            gap: value.is_none(),
        });
        previous = value.map(|_| (time, x, y));
    }
    if count > 0 {
        report.timeline = Some(Timeline {
            points,
            zero: units.format(0),
            middle: units.format_file(axis_max / 2),
            maximum: units.format_file(axis_max),
            first: date(first),
            last: if first == last {
                String::new()
            } else {
                date(last)
            },
        });
    }
    if count < 2 || first == last {
        report.scope.push('\n');
        report.scope.push_str(tr!(
            "Keep complete measures of this scope at two different times to see a trend."
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: u64, time: i64, logical: u64) -> Saved {
        Saved {
            directory: format!("inv-{id}").into(),
            context: AcquisitionContext {
                inventory: InventoryId(u128::from(id)),
                generation: GenerationId(1),
                source: SourceId(1),
                scan: ScanId(id),
                policy: PolicyId(1),
                mount: MountEpoch(1),
                run: RunEpoch(1),
                validation: ValidationEpoch(1),
            },
            root: "native-root".into(),
            started: time,
            measured: Some(time),
            coverage: ScopeCoverage::Complete,
            logical: Some(logical),
            unknown: 0,
            continuity: Some([2; 32]),
        }
    }

    #[test]
    fn timeline_uses_elapsed_time_and_breaks_at_partial_or_replaced_roots() {
        let first = saved(1, 10, 1000);
        let mut partial = saved(2, 20, 700);
        partial.coverage = ScopeCoverage::Partial;
        let next = saved(3, 30, 7000);
        let mut replaced = saved(4, 40, 9000);
        replaced.continuity = Some([3; 32]);
        let later = saved(5, 50, 6000);
        let last = saved(6, 90, 8000);
        let mut automatic = saved(7, 100, 10000);
        automatic.directory = "auto-example.sqlite".into();
        let mut elsewhere = saved(8, 1, 10000);
        elsewhere.root = "another-root".into();
        let report = evolution(
            &first,
            &[
                last,
                next,
                later,
                first.clone(),
                first.clone(),
                partial,
                replaced,
                automatic,
                elsewhere,
            ],
            Units::Decimal,
        );
        let points = report.timeline.unwrap().points;
        assert_eq!(report.rows.len(), 6);
        assert_eq!(
            points.iter().map(|p| p.x).collect::<Vec<_>>(),
            [0., 0.125, 0.25, 0.375, 0.5, 1.]
        );
        assert_eq!(
            points.iter().map(|p| p.gap).collect::<Vec<_>>(),
            [false, true, false, true, false, false]
        );
        assert_eq!(
            points.iter().map(|p| p.connect).collect::<Vec<_>>(),
            [false, false, false, false, false, true]
        );
        assert_eq!(points[0].y, 0.875);
        assert_eq!(points[5].y, 0.);
    }

    #[test]
    fn old_inventory_without_native_proof_is_still_listed_without_a_curve() {
        let mut legacy = saved(1, 10, 1000);
        legacy.continuity = None;
        let report = evolution(&legacy, std::slice::from_ref(&legacy), Units::Binary);
        assert!(report.timeline.is_none());
        assert_eq!(report.rows.len(), 1);
    }
}
