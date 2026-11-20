use super::*;

/// A small rendering check: source weights remain visible at the minimum panel
/// width, with the names/sizes in their own row and no fill for unknown weights.
#[test]
fn folder_weight_gauges_render_at_narrow_and_wide_panel_widths() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    window.set_rows(
        Rc::new(slint::VecModel::from(vec![
            TreeRow {
                node_id: 1,
                name: "Photographies".into(),
                size: "75 MiB".into(),
                has_weight: true,
                fraction: 0.75,
                proportion: "≈75 %".into(),
                weight_provisional: true,
                selected: true,
                ..Default::default()
            },
            TreeRow {
                node_id: 2,
                name: "Documents au nom particulièrement long".into(),
                size: "25 MiB".into(),
                has_weight: true,
                fraction: 0.25,
                proportion: "25 %".into(),
                color_index: 1,
                ..Default::default()
            },
            TreeRow {
                node_id: 3,
                name: "Dossier non mesuré".into(),
                size: "—".into(),
                proportion: "—".into(),
                color_index: 2,
                ..Default::default()
            },
        ]))
        .into(),
    );
    window.show().unwrap();
    let (width, height) = (1200, 760);
    adapter.set_size(slint::PhysicalSize::new(width, height));
    for theme in [1, 2] {
        window.set_theme_choice(theme);
        for panel_width in [220, 360] {
            window.set_sidebar_width(panel_width as f32);
            slint::platform::update_timers_and_animations();
            let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
            assert!(adapter.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, width as usize);
            }));
            assert_eq!(window.get_actual_sidebar_width(), panel_width as f32);
            let count = |rgb: (u8, u8, u8)| {
                pixels
                    .iter()
                    .filter(|pixel| (pixel.r, pixel.g, pixel.b) == rgb)
                    .count()
            };
            let larger = count((0x23, 0x65, 0x8a));
            let smaller = count((0x38, 0x6b, 0x47));
            assert!(larger > 30, "75% gauge is visible");
            assert!(smaller > 8, "25% gauge is visible");
            // Provisional hatching removes colored pixels, not logical width.
            // Compare rendered extents so the oracle measures the 75/25 weights.
            let colored_width = |rgb: (u8, u8, u8)| {
                let mut bounds: Option<(usize, usize)> = None;
                for (index, pixel) in pixels.iter().enumerate() {
                    if (pixel.r, pixel.g, pixel.b) == rgb {
                        let x = index % width as usize;
                        bounds = Some(
                            bounds.map_or((x, x), |(first, last)| (first.min(x), last.max(x))),
                        );
                    }
                }
                bounds.map_or(0, |(first, last)| last - first + 1)
            };
            let wide = colored_width((0x23, 0x65, 0x8a));
            let narrow = colored_width((0x38, 0x6b, 0x47));
            assert!(
                wide * 2 > narrow * 5 && wide * 2 < narrow * 7,
                "theme={theme}, sidebar={panel_width}, widths={wide}/{narrow}"
            );
            assert_eq!(count((0x70, 0x4e, 0x89)), 0, "unknown has no area");
            if let Some(output) = std::env::var_os("BUNNY_GAUGE_CAPTURE_DIR") {
                use std::io::Write;
                fs::create_dir_all(&output).unwrap();
                let name = format!("folder-gauges-{panel_width}-theme-{theme}.ppm");
                let mut file = std::io::BufWriter::new(
                    fs::File::create(std::path::Path::new(&output).join(name)).unwrap(),
                );
                write!(file, "P6\n{width} {height}\n255\n").unwrap();
                for pixel in &pixels {
                    file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
                }
            }
        }
    }
    assert_eq!(window.get_rows().row_data(0).unwrap().name, "Photographies");
    assert_eq!(window.get_rows().row_data(0).unwrap().size, "75 MiB");
    window.hide().unwrap();
}

#[test]
fn folder_share_cells_keep_colored_geometry_and_neutral_unknowns_in_both_themes() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(adapter.clone()))).unwrap();
    let (window, _controller) = new_session(Default::default(), Some("fr_FR".into())).unwrap();
    for column in 1..11 {
        window.invoke_set_column(column, column == 4);
    }
    let share_width = window.get_column_settings().row_data(4).unwrap().pixels;
    window.set_details(
        Rc::new(slint::VecModel::from(vec![
            DetailRow {
                node_id: 1,
                name: "Grand dossier".into(),
                size: "75 MiB".into(),
                has_weight: true,
                fraction: 0.75,
                proportion: "≈ 75,0 %".into(),
                weight_provisional: true,
                color_index: 0,
                ..Default::default()
            },
            DetailRow {
                node_id: 2,
                name: "Petit fichier".into(),
                size: "25 MiB".into(),
                has_weight: true,
                fraction: 0.25,
                proportion: "25,0 %".into(),
                color_index: 1,
                ..Default::default()
            },
            DetailRow {
                node_id: 3,
                name: "Inconnu".into(),
                size: "—".into(),
                proportion: "—".into(),
                color_index: 2,
                ..Default::default()
            },
            DetailRow {
                node_id: 4,
                name: "Vide".into(),
                size: "0 octet".into(),
                has_weight: true,
                fraction: 0.,
                proportion: "0,0 %".into(),
                color_index: 3,
                ..Default::default()
            },
        ]))
        .into(),
    );
    window.show().unwrap();
    let (width, height) = (1200, 760);
    adapter.set_size(slint::PhysicalSize::new(width, height));
    for theme in [1, 2] {
        window.set_theme_choice(theme);
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
        assert!(adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width as usize);
        }));
        let colored_bounds = |rgb: (u8, u8, u8)| {
            let xs: Vec<_> = pixels
                .iter()
                .enumerate()
                .filter(|(_, pixel)| (pixel.r, pixel.g, pixel.b) == rgb)
                .map(|(index, _)| index % width as usize)
                .collect();
            xs.iter()
                .min()
                .zip(xs.iter().max())
                .map(|(first, last)| (*first, *last))
        };
        let colored_width = |rgb| colored_bounds(rgb).map_or(0, |(first, last)| last - first + 1);
        assert_eq!(
            colored_bounds((0x23, 0x65, 0x8a)).unwrap().0,
            colored_bounds((0x38, 0x6b, 0x47)).unwrap().0,
            "all percentage fills start at the left edge of their cell"
        );
        assert!((colored_width((0x23, 0x65, 0x8a)) as f32 - share_width * 0.75).abs() <= 2.);
        assert!((colored_width((0x38, 0x6b, 0x47)) as f32 - share_width * 0.25).abs() <= 2.);
        assert_eq!(
            colored_width((0x70, 0x4e, 0x89)),
            0,
            "unknown has no invented fill"
        );
        assert_eq!(
            colored_width((0x8a, 0x4b, 0x35)),
            0,
            "zero has no minimum fill"
        );
        if let Some(output) = std::env::var_os("BUNNY_GAUGE_CAPTURE_DIR") {
            use std::io::Write;
            fs::create_dir_all(&output).unwrap();
            let mut file = std::io::BufWriter::new(
                fs::File::create(
                    std::path::Path::new(&output).join(format!("folder-share-theme-{theme}.ppm")),
                )
                .unwrap(),
            );
            write!(file, "P6\n{width} {height}\n255\n").unwrap();
            for pixel in &pixels {
                file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
            }
        }
    }
    window.hide().unwrap();
}
