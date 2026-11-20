//! VIEW-002/008/012: pointer hit boxes, tooltip bounds and unchanged selection.
use super::*;
use slint::platform::{PointerEventButton, WindowEvent};

fn render_hover(window: &SortSizeWindow, adapter: &MinimalSoftwareWindow) -> Vec<slint::Rgb8Pixel> {
    slint::platform::update_timers_and_animations();
    window.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 1200 * 760];
    assert!(adapter.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 1200);
    }));
    pixels
}

fn move_to(window: &SortSizeWindow, x: f32, y: f32) {
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(
            window.get_map_origin_x() + x,
            window.get_map_origin_y() + y,
        ),
    });
}

fn assert_bounded_tooltip(window: &SortSizeWindow) {
    assert!(window.get_map_tooltip_visible());
    let (x, y, width, height) = (
        window.get_map_tooltip_x(),
        window.get_map_tooltip_y(),
        window.get_map_tooltip_width(),
        window.get_map_tooltip_height(),
    );
    assert!(x >= 0. && y >= 0., "tooltip starts in window: {x},{y}");
    assert!(x + width <= 1200.5, "tooltip right edge: {}", x + width);
    assert!(y + height <= 760.5, "tooltip lower edge: {}", y + height);
}

#[test]
fn pointer_hover_identifies_one_and_two_pixel_tiles_in_both_modes_without_selecting() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _controller) = new_session(Default::default(), Some("en_GB".into())).unwrap();
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1200, 760));
    render_hover(&window, &adapter);
    let long_name = format!("photographies-{}-complete-name.bin", "étiquette".repeat(28));
    let full_path = format!(
        "C:\\source\\{}\\{}",
        "répertoire-long\\".repeat(10),
        long_name
    );
    let clicked = Rc::new(std::cell::Cell::new(-1));
    let click_observer = clicked.clone();
    window.on_activate_tile(move |index| click_observer.set(index));
    for theme in [1, 2] {
        window.set_theme_choice(theme);
        for mode in [0, 1] {
            window.set_treemap_mode(mode);
            render_hover(&window, &adapter);
            let map_width = window.get_map_width();
            let map_height = window.get_map_height();
            assert!(map_width > 100. && map_height > 20.);
            let model = Rc::new(VecModel::from(vec![
                MapTile {
                    node_id: 40,
                    name: "Large directory".into(),
                    size: "75 MiB".into(),
                    directory: true,
                    w: (map_width - 3.) / map_width,
                    h: 1.,
                    band_w: (map_width - 3.) / map_width,
                    ..Default::default()
                },
                MapTile {
                    node_id: 41,
                    name: long_name.clone().into(),
                    path: full_path.clone().into(),
                    size: "≥ 2 bytes".into(),
                    partial: true,
                    color_index: 1,
                    x: (map_width - 3.) / map_width,
                    w: 2. / map_width,
                    h: 1.,
                    band_x: (map_width - 3.) / map_width,
                    band_w: 2. / map_width,
                    ..Default::default()
                },
                MapTile {
                    node_id: 42,
                    name: "One pixel".into(),
                    size: "1 byte".into(),
                    color_index: 2,
                    x: (map_width - 1.) / map_width,
                    w: 1. / map_width,
                    h: 1.,
                    band_x: (map_width - 1.) / map_width,
                    band_w: 1. / map_width,
                    ..Default::default()
                },
            ]));
            window.set_tiles(model.clone().into());
            window.set_map_cursor(0);
            window.set_inspected_node(-1);
            render_hover(&window, &adapter);
            for (x, y, index) in [
                (2., 2., 0),
                (map_width - 2.5, 2., 1),
                (map_width - 1.5, map_height - 1., 1),
                (map_width - 0.5, map_height - 0.5, 2),
            ] {
                eprintln!(
                    "pointer test before: origin={},{} map={}x{} intended={x},{y} expect={index}",
                    window.get_map_origin_x(),
                    window.get_map_origin_y(),
                    window.get_map_width(),
                    window.get_map_height()
                );
                move_to(&window, x, y);
                let pixels = render_hover(&window, &adapter);
                eprintln!(
                    "pointer test after: origin={},{} map={}x{} hover={} tooltip={},{} {}x{}",
                    window.get_map_origin_x(),
                    window.get_map_origin_y(),
                    window.get_map_width(),
                    window.get_map_height(),
                    window.get_map_hover_index(),
                    window.get_map_tooltip_x(),
                    window.get_map_tooltip_y(),
                    window.get_map_tooltip_width(),
                    window.get_map_tooltip_height()
                );
                assert_eq!(
                    window.get_map_hover_index(),
                    index,
                    "theme={theme} mode={mode} x={x}"
                );
                assert_bounded_tooltip(&window);
                assert!(
                    window.get_map_tooltip_text().contains(if index == 0 {
                        "Folder"
                    } else {
                        "File"
                    }),
                    "tooltip identifies file or directory"
                );
                assert_eq!(
                    window.get_map_cursor(),
                    0,
                    "hover must not move keyboard cursor"
                );
                assert_eq!(
                    window.get_inspected_node(),
                    -1,
                    "hover must not inspect/select"
                );
                assert!(
                    std::ptr::eq(
                        model.as_ref(),
                        window
                            .get_tiles()
                            .as_any()
                            .downcast_ref::<VecModel<MapTile>>()
                            .unwrap(),
                    ),
                    "moving the pointer must preserve the tile model"
                );
                if index == 1 {
                    let text = window.get_map_tooltip_text();
                    assert!(
                        text.contains(&long_name),
                        "full unelided name is in tooltip"
                    );
                    assert!(text.contains(&full_path), "full path is in tooltip");
                    assert!(text.contains("2 bytes"));
                    assert!(
                        text.contains("Partial"),
                        "provisional measurement is explicit"
                    );
                    if let Some(output) = std::env::var_os("BUNNY_HOVER_CAPTURE_DIR") {
                        use std::io::Write;
                        fs::create_dir_all(&output).unwrap();
                        let path = std::path::Path::new(&output).join(format!(
                            "hover-theme-{theme}-mode-{mode}-y-{}.ppm",
                            y as u32
                        ));
                        let mut file = std::io::BufWriter::new(fs::File::create(path).unwrap());
                        write!(file, "P6\n1200 760\n255\n").unwrap();
                        for pixel in pixels {
                            file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
                        }
                    }
                }
            }
            let point = slint::LogicalPosition::new(
                window.get_map_origin_x() + map_width - 0.5,
                window.get_map_origin_y() + map_height - 0.5,
            );
            clicked.set(-1);
            window.window().dispatch_event(WindowEvent::PointerPressed {
                position: point,
                button: PointerEventButton::Left,
            });
            window
                .window()
                .dispatch_event(WindowEvent::PointerReleased {
                    position: point,
                    button: PointerEventButton::Left,
                });
            assert_eq!(
                clicked.get(),
                2,
                "tooltip must not intercept click on one-pixel tile"
            );
            window.window().dispatch_event(WindowEvent::PointerExited);
            render_hover(&window, &adapter);
            assert_eq!(window.get_map_hover_index(), -1);
            assert!(!window.get_map_tooltip_visible());
        }
    }
    // A progressive update can move the surface under an unmoving pointer.
    // Publications replace the tile model, as does changing the viewed folder.
    // No pointer movement is needed for that replacement or a view mode change.
    window.set_treemap_mode(0);
    let stationary = Rc::new(VecModel::from(vec![
        MapTile {
            node_id: 70,
            name: "First".into(),
            w: 0.5,
            h: 1.,
            band_w: 0.5,
            ..Default::default()
        },
        MapTile {
            node_id: 71,
            name: "Second".into(),
            x: 0.5,
            w: 0.5,
            h: 1.,
            band_x: 0.5,
            band_w: 0.5,
            ..Default::default()
        },
    ]));
    window.set_tiles(stationary.clone().into());
    render_hover(&window, &adapter);
    move_to(&window, window.get_map_width() * 0.25, 10.);
    render_hover(&window, &adapter);
    assert_eq!(window.get_map_hover_index(), 0);
    window.set_tiles(
        Rc::new(VecModel::from(vec![
            MapTile {
                x: 0.5,
                ..stationary.row_data(0).unwrap()
            },
            MapTile {
                x: 0.,
                ..stationary.row_data(1).unwrap()
            },
        ]))
        .into(),
    );
    render_hover(&window, &adapter);
    assert_eq!(
        window.get_map_hover_index(),
        1,
        "stationary hover follows progressive geometry updates"
    );
    assert!(window.get_map_tooltip_text().contains("Second"));
    window.set_treemap_mode(1);
    render_hover(&window, &adapter);
    assert_eq!(
        window.get_map_hover_index(),
        0,
        "stationary hover follows layout mode change"
    );
    window.set_tiles(
        Rc::new(VecModel::from(vec![MapTile {
            node_id: 80,
            name: "Replacement folder".into(),
            w: 1.,
            h: 1.,
            band_w: 1.,
            ..Default::default()
        }]))
        .into(),
    );
    render_hover(&window, &adapter);
    assert_eq!(window.get_map_hover_index(), 0);
    assert!(window.get_map_tooltip_text().contains("Replacement folder"));
    window.set_tiles(Rc::new(VecModel::<MapTile>::from(vec![])).into());
    render_hover(&window, &adapter);
    assert_eq!(window.get_map_hover_index(), -1);
    assert!(!window.get_map_tooltip_visible());
    window.hide().unwrap();
}
