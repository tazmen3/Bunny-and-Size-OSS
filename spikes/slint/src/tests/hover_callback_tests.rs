use super::*;

#[test]
fn pointer_hover_reads_current_tiles_without_selecting_or_rebuilding_them() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter))).unwrap();
    let window = new_window().unwrap();
    let _controller = connect(&window);
    window.set_map_width(1000.);
    window.set_map_height(100.);
    let tiles = Rc::new(VecModel::from(vec![
        MapTile {
            node_id: 42,
            name: "large".into(),
            w: 0.999,
            h: 1.,
            band_w: 0.999,
            ..Default::default()
        },
        MapTile {
            node_id: 43,
            name: "one pixel".into(),
            x: 0.999,
            w: 0.001,
            h: 1.,
            band_x: 0.999,
            band_w: 0.001,
            ..Default::default()
        },
    ]));
    window.set_tiles(tiles.clone().into());
    window.set_map_cursor(0);
    window.set_inspected_node(42);
    for bands in [false, true] {
        window.set_treemap_mode(i32::from(bands));
        window.invoke_map_hover(999.5, 50., true);
        assert_eq!(window.get_map_hover_index(), 1);
        assert_eq!(window.get_map_cursor(), 0);
        assert_eq!(window.get_inspected_node(), 42);
        assert_eq!(window.get_tiles().row_data(1).unwrap().node_id, 43);
        // Holding the same VecModel proves that pointer movement does not call
        // the layout/projector or publish a replacement inventory model.
        tiles.set_row_data(
            1,
            MapTile {
                node_id: 44,
                ..tiles.row_data(1).unwrap()
            },
        );
        assert_eq!(window.get_tiles().row_data(1).unwrap().node_id, 44);
        window.invoke_map_hover(999.5, 50., false);
        assert_eq!(window.get_map_hover_index(), -1);
        tiles.set_row_data(
            1,
            MapTile {
                node_id: 43,
                ..tiles.row_data(1).unwrap()
            },
        );
    }
}
