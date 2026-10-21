//! Pure, bounded squarified layout. Areas describe observed logical bytes only.
//! No filesystem access, native path conversion or Slint dependency here.

pub(crate) const MAX_TILES: usize = 256;
// Elements below 0.1% are grouped even when the rectangle budget permits more.
const MIN_FRACTION: f64 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    Node(usize),
    Other,
    Omitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Weight {
    pub target: Target,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Projection {
    pub weights: Box<[Weight]>,
    pub grouped: usize,
    pub zeros: usize,
    pub unknown: usize,
    pub partial: bool,
    pub inconsistent: bool,
}

impl Projection {
    /// Called on the scan worker. Temporary storage is bounded by the retained
    /// inventory; final storage is <= min(children + 1, MAX_TILES) per folder.
    pub fn new(
        children: impl Iterator<Item = (usize, Option<u64>, bool)>,
        total: Option<u64>,
        incomplete: bool,
        truncated: bool,
    ) -> Self {
        let mut result = Self {
            partial: incomplete,
            ..Default::default()
        };
        let mut known = Vec::new();
        let mut sum = 0u128;
        for (index, bytes, partial) in children {
            result.partial |= partial;
            match bytes {
                Some(0) if !partial => result.zeros += 1,
                None | Some(0) => result.unknown += 1,
                Some(bytes) => {
                    sum += u128::from(bytes);
                    known.push(Weight {
                        target: Target::Node(index),
                        bytes,
                    });
                }
            }
        }
        if sum > u128::from(u64::MAX) {
            result.inconsistent = true;
            return result;
        }
        // A difference can be attributed to omissions only if all retained
        // sizes are available. Never invent an area for unknown entries.
        let omitted = if let Some(total) = total {
            result.inconsistent = sum > u128::from(total)
                || (!truncated && result.unknown == 0 && sum != u128::from(total));
            if truncated && result.unknown == 0 {
                total.saturating_sub(sum as u64)
            } else {
                0
            }
        } else {
            result.unknown += 1;
            0
        };
        if result.inconsistent {
            return result; // Invalid aggregates must not produce plausible proportions.
        }
        known.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then(a.target.cmp(&b.target)));
        let denominator = (sum + u128::from(omitted)) as f64;
        let mut displayed = Vec::with_capacity(known.len().min(MAX_TILES));
        let mut other = 0u64;
        for weight in known {
            // Reserve one slot for Other and one for omitted bytes.
            if displayed.len() < MAX_TILES - 2 && weight.bytes as f64 / denominator >= MIN_FRACTION
            {
                displayed.push(weight);
            } else {
                other += weight.bytes; // Successful inventory totals fit u64.
                result.grouped += 1;
            }
        }
        if other > 0 {
            displayed.push(Weight {
                target: Target::Other,
                bytes: other,
            });
        }
        if omitted > 0 {
            displayed.push(Weight {
                target: Target::Omitted,
                bytes: omitted,
            });
        }
        displayed.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then(a.target.cmp(&b.target)));
        result.weights = displayed.into_boxed_slice();
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Squarified rows along the shorter edge, largest weights first. Coordinates
/// are normalized for Slint; the actual aspect ratio is used for squarification.
/// Invalid dimensions or weights return no geometry, rather than NaN or infinity.
pub(crate) fn layout(weights: &[Weight], width: f64, height: f64) -> Vec<Rect> {
    if weights.is_empty()
        || weights.len() > MAX_TILES
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || weights.iter().any(|w| w.bytes == 0)
    {
        return Vec::new();
    }
    // Scale down before multiplying dimensions, including extreme valid inputs.
    let scale = width.max(height);
    let width = width / scale;
    let height = height / scale;
    let area = width * height;
    if area == 0.0 {
        return Vec::new();
    }
    let total: f64 = weights.iter().map(|w| w.bytes as f64).sum();
    let areas: Vec<f64> = weights
        .iter()
        .map(|w| w.bytes as f64 / total * area)
        .collect();
    let mut bounds = Rect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    };
    let mut output = Vec::with_capacity(weights.len());
    let mut start = 0;
    while start < areas.len() {
        let side = bounds.width.min(bounds.height);
        if side <= 0.0 {
            // The remaining positive fraction can be below f64 resolution (e.g.
            // 1 byte beside u64::MAX). Do not give it an invented minimum area.
            output.resize(
                weights.len(),
                Rect {
                    x: (bounds.x / width).clamp(0.0, 1.0),
                    y: (bounds.y / height).clamp(0.0, 1.0),
                    width: 0.0,
                    height: 0.0,
                },
            );
            break;
        }
        let mut end = start + 1;
        let mut sum = areas[start];
        let mut min = areas[start];
        let mut max = min;
        let mut score = worst(sum, min, max, side);
        while end < areas.len() {
            let next = areas[end];
            let candidate = worst(sum + next, min.min(next), max.max(next), side);
            if candidate > score {
                break;
            }
            sum += next;
            min = min.min(next);
            max = max.max(next);
            score = candidate;
            end += 1;
        }
        let vertical = bounds.width >= bounds.height;
        let thickness = if end == areas.len() {
            if vertical {
                bounds.width
            } else {
                bounds.height
            }
        } else {
            (sum / side).min(if vertical {
                bounds.width
            } else {
                bounds.height
            })
        };
        let mut cursor = if vertical { bounds.y } else { bounds.x };
        let edge = if vertical {
            bounds.y + bounds.height
        } else {
            bounds.x + bounds.width
        };
        for (offset, value) in areas[start..end].iter().enumerate() {
            let length = if start + offset + 1 == end {
                edge - cursor
            } else {
                (value / thickness).min(edge - cursor)
            }
            .max(0.0);
            let rect = if vertical {
                Rect {
                    x: bounds.x,
                    y: cursor,
                    width: thickness,
                    height: length,
                }
            } else {
                Rect {
                    x: cursor,
                    y: bounds.y,
                    width: length,
                    height: thickness,
                }
            };
            output.push(Rect {
                x: (rect.x / width).clamp(0.0, 1.0),
                y: (rect.y / height).clamp(0.0, 1.0),
                width: (rect.width / width).clamp(0.0, 1.0 - (rect.x / width).clamp(0.0, 1.0)),
                height: (rect.height / height).clamp(0.0, 1.0 - (rect.y / height).clamp(0.0, 1.0)),
            });
            cursor += length;
        }
        if vertical {
            bounds.x += thickness;
            bounds.width = (bounds.width - thickness).max(0.0);
        } else {
            bounds.y += thickness;
            bounds.height = (bounds.height - thickness).max(0.0);
        }
        start = end;
    }
    if output
        .iter()
        .any(|r| ![r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite()))
    {
        return Vec::new();
    }
    output
}
fn worst(sum: f64, min: f64, max: f64, side: f64) -> f64 {
    let square = sum * sum;
    (side * side * max / square).max(square / (side * side * min))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn weights(values: &[u64]) -> Vec<Weight> {
        values
            .iter()
            .enumerate()
            .map(|(i, &bytes)| Weight {
                target: Target::Node(i),
                bytes,
            })
            .collect()
    }
    fn assert_geometry(weights: &[Weight], width: f64, height: f64) {
        let rects = layout(weights, width, height);
        assert_eq!(rects.len(), weights.len());
        assert_eq!(rects, layout(weights, width, height));
        let total: f64 = weights.iter().map(|w| w.bytes as f64).sum();
        for (i, rect) in rects.iter().enumerate() {
            assert!(
                [rect.x, rect.y, rect.width, rect.height]
                    .iter()
                    .all(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0)
            );
            assert!(rect.x + rect.width <= 1.0 + 1e-12);
            assert!(rect.y + rect.height <= 1.0 + 1e-12);
            assert!((rect.width * rect.height - weights[i].bytes as f64 / total).abs() < 1e-12);
            for other in &rects[..i] {
                let overlap =
                    (rect.x + rect.width).min(other.x + other.width) - rect.x.max(other.x);
                let vertical =
                    (rect.y + rect.height).min(other.y + other.height) - rect.y.max(other.y);
                assert!(overlap <= 1e-12 || vertical <= 1e-12);
            }
        }
        assert!((rects.iter().map(|r| r.width * r.height).sum::<f64>() - 1.0).abs() < 1e-12);
    }
    #[test]
    fn known_proportions_determinism_bounds_and_non_overlap() {
        for values in [
            vec![6, 3, 1],
            vec![1],
            vec![1; 256],
            vec![u64::MAX - 2, 1, 1],
            (1..=256).rev().collect(),
        ] {
            for (w, h) in [(1000.0, 400.0), (320.0, 900.0), (1.0, 1.0), (1e100, 1e100)] {
                assert_geometry(&weights(&values), w, h);
            }
        }
    }
    #[test]
    fn varied_deterministic_corpora_preserve_proportions() {
        let mut state = 7u64;
        for count in [2, 7, 31, 127, 256] {
            for _ in 0..8 {
                let mut values: Vec<_> = (0..count)
                    .map(|_| {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        1 + state % 1_000_000
                    })
                    .collect();
                values.sort_unstable_by(|a, b| b.cmp(a));
                assert_geometry(&weights(&values), 841.0, 190.0);
                assert_geometry(&weights(&values), 190.0, 841.0);
            }
        }
    }
    #[test]
    fn invalid_dimensions_and_zero_weights_have_no_area() {
        for size in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(layout(&weights(&[1]), size, 1.0).is_empty());
            assert!(layout(&weights(&[1]), 1.0, size).is_empty());
        }
        assert!(layout(&weights(&[0]), 1.0, 1.0).is_empty());
        assert!(layout(&weights(&[1; 257]), 1.0, 1.0).is_empty());
    }
    #[test]
    fn bounded_groups_preserve_all_observed_bytes() {
        let projection = Projection::new(
            (0..50_000).map(|i| (i, Some(1), false)),
            Some(50_007),
            false,
            true,
        );
        assert!(projection.weights.len() <= MAX_TILES);
        assert_eq!(projection.grouped, 50_000);
        assert_eq!(
            projection.weights.iter().map(|w| w.bytes).sum::<u64>(),
            50_007
        );
        assert!(projection.weights.contains(&Weight {
            target: Target::Omitted,
            bytes: 7
        }));
        assert_geometry(&projection.weights, 700.0, 220.0);
    }
    #[test]
    fn unknown_zero_partial_and_inconsistent_totals_are_distinct() {
        let p = Projection::new(
            [
                (0, None, false),
                (1, Some(0), false),
                (2, Some(0), true),
                (3, Some(3), true),
            ]
            .into_iter(),
            Some(8),
            true,
            true,
        );
        assert_eq!((p.zeros, p.unknown, p.partial), (1, 2, true));
        assert_eq!(
            p.weights,
            weights(&[3])
                .into_iter()
                .map(|mut w| {
                    w.target = Target::Node(3);
                    w
                })
                .collect::<Vec<_>>()
                .into_boxed_slice()
        );
        assert!(
            Projection::new([(0, Some(9), false)].into_iter(), Some(8), false, false).inconsistent
        );
        let p = Projection::new(std::iter::empty(), Some(0), false, false);
        assert!(p.weights.is_empty());
    }
    #[test]
    fn input_order_does_not_change_projection() {
        let a = [
            (3, Some(2), false),
            (1, Some(2), false),
            (2, Some(6), false),
        ];
        assert_eq!(
            Projection::new(a.into_iter(), Some(10), false, false).weights,
            Projection::new(a.into_iter().rev(), Some(10), false, false).weights
        );
    }
}
