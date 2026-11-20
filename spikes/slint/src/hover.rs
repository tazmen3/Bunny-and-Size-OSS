//! Read-only pointer hit testing against the geometry already displayed by Slint.
use crate::MapTile;
use slint::{Model, ModelRc};

/// Use the same multiplication and half-open bounds as the drawn rectangles.
/// There is no minimum hit width: a one-pixel tile keeps its real surface, and
/// zero-area tiles never steal their neighbour's hover. Reverse iteration agrees
/// with Slint's paint order if rounding or supplied geometry causes an overlap.
pub(crate) fn hit_test(
    tiles: &ModelRc<MapTile>,
    dimensions: (f32, f32),
    point: (f32, f32),
    bands: bool,
) -> Option<usize> {
    let (width, height) = dimensions;
    let (x, y) = point;
    if ![width, height, x, y].into_iter().all(f32::is_finite)
        || width <= 0.
        || height <= 0.
        || x < 0.
        || y < 0.
        || x >= width
        || y >= height
    {
        return None;
    }
    (0..tiles.row_count()).rev().find(|&index| {
        let Some(tile) = tiles.row_data(index) else {
            return false;
        };
        let (left, top, tile_width, tile_height) = if bands {
            (width * tile.band_x, 0., width * tile.band_w, height)
        } else {
            (
                width * tile.x,
                height * tile.y,
                width * tile.w,
                height * tile.h,
            )
        };
        tile_width > 0.
            && tile_height > 0.
            && x >= left
            && x < left + tile_width
            && y >= top
            && y < top + tile_height
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn model(tiles: Vec<MapTile>) -> ModelRc<MapTile> {
        Rc::new(slint::VecModel::from(tiles)).into()
    }

    #[test]
    fn exact_hit_bounds_include_one_and_two_pixel_tiles_in_both_modes() {
        let tiles = model(
            [(0., 0.997), (0.997, 0.001), (0.998, 0.002)]
                .into_iter()
                .enumerate()
                .map(|(index, (x, w))| MapTile {
                    node_id: index as i32,
                    x,
                    w,
                    h: 1.,
                    band_x: x,
                    band_w: w,
                    ..Default::default()
                })
                .collect(),
        );
        for bands in [false, true] {
            for (x, expected) in [(996.5, 0), (997.5, 1), (998.5, 2), (999.5, 2)] {
                assert_eq!(
                    hit_test(&tiles, (1000., 100.), (x, 50.), bands),
                    Some(expected)
                );
            }
            for outside in [(-0.1, 50.), (1000., 50.), (500., -0.1), (500., 100.)] {
                assert_eq!(hit_test(&tiles, (1000., 100.), outside, bands), None);
            }
        }
    }

    #[test]
    fn geometry_modes_holes_and_zero_areas_do_not_invent_hover_targets() {
        let tiles = model(vec![
            MapTile {
                x: 0.,
                y: 0.,
                w: 0.5,
                h: 0.5,
                band_x: 0.,
                band_w: 0.25,
                ..Default::default()
            },
            MapTile {
                x: 0.5,
                y: 0.5,
                w: 0.5,
                h: 0.5,
                band_x: 0.25,
                band_w: 0.75,
                ..Default::default()
            },
            MapTile {
                x: 0.,
                y: 0.,
                w: 0.,
                h: 1.,
                band_x: 0.,
                band_w: 0.,
                ..Default::default()
            },
        ]);
        assert_eq!(hit_test(&tiles, (100., 100.), (10., 90.), false), None);
        assert_eq!(hit_test(&tiles, (100., 100.), (10., 90.), true), Some(0));
        assert_eq!(hit_test(&tiles, (100., 100.), (50., 50.), false), Some(1));
        assert_eq!(hit_test(&tiles, (100., 100.), (50., 10.), false), None);
        assert_eq!(hit_test(&tiles, (100., 100.), (25., 10.), true), Some(1));
        assert_eq!(hit_test(&tiles, (0., 100.), (0., 10.), true), None);
        assert_eq!(hit_test(&tiles, (100., 100.), (f32::NAN, 10.), true), None);
    }

    #[test]
    fn last_painted_rectangle_wins_only_on_its_actual_surface() {
        let tiles = model(vec![
            MapTile {
                w: 1.,
                h: 1.,
                band_w: 1.,
                ..Default::default()
            },
            MapTile {
                x: 0.5,
                w: 0.01,
                h: 1.,
                band_x: 0.5,
                band_w: 0.01,
                ..Default::default()
            },
        ]);
        for bands in [false, true] {
            assert_eq!(hit_test(&tiles, (100., 100.), (50.5, 10.), bands), Some(1));
            assert_eq!(hit_test(&tiles, (100., 100.), (49.9, 10.), bands), Some(0));
            assert_eq!(hit_test(&tiles, (100., 100.), (51., 10.), bands), Some(0));
        }
    }
}
