use super::*;
use crate::locale::Language;
use slint::{ComponentHandle, Timer, TimerMode};
use std::io::Write;

// Native raster snapshots and measured viewport, not a headless layout claim.
#[test]
#[ignore = "native Winit; explicit SORTSIZE_BRANDING_OUTPUT and SLINT_SCALE_FACTOR required"]
fn native_about_all_languages_at_minimum_and_preferred_sizes() {
    let output = std::path::PathBuf::from(std::env::var_os("SORTSIZE_BRANDING_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let window = LegalWindow::new().unwrap();
    window.set_product_version(env!("CARGO_PKG_VERSION").into());
    window.set_license_text(include_str!("../../../../LICENSE").into());
    let weak = window.as_weak();
    let timer = Timer::default();
    let mut phase = 0usize;
    let mut records = String::new();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let window = weak.upgrade().unwrap();
        let case = phase / 2;
        if case == 20 {
            fs::write(output.join("geometry.tsv"), &records).unwrap();
            window.hide().unwrap();
            slint::quit_event_loop().unwrap();
            return;
        }
        let language = Language::ALL[case / 4];
        let (width, height) = if case % 4 < 2 {
            (580., 400.)
        } else {
            (800., 560.)
        };
        if phase.is_multiple_of(2) {
            crate::locale::activate_bundled(language).unwrap();
            let context = presentation::Context {
                language,
                generation: 1,
            };
            let _scope = context.enter();
            window.set_notices(
                tr!(include_str!("../legal-notice-en.txt"))
                    .replace("@VERSION@", env!("CARGO_PKG_VERSION"))
                    .into(),
            );
            window.set_full_license(case % 2 == 1);
            let scale = window.window().scale_factor();
            window.window().set_size(slint::PhysicalSize::new(
                (width * scale).round() as u32,
                (height * scale).round() as u32,
            ));
        } else {
            let pixels = window.window().take_snapshot().unwrap();
            let name = format!("{}-{}-{}.ppm", language.as_str(), height as u32, case % 2);
            let mut file = fs::File::create(output.join(name)).unwrap();
            write!(file, "P6\n{} {}\n255\n", pixels.width(), pixels.height()).unwrap();
            for pixel in pixels.as_slice() {
                file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
            }
            records += &format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                language.as_str(),
                width,
                height,
                case % 2,
                pixels.width(),
                pixels.height(),
                window.get_notice_viewport_height()
            );
            assert!(
                window.get_notice_viewport_height() >= 100.,
                "legal text viewport too small: {}",
                window.get_notice_viewport_height()
            );
        }
        phase += 1;
    });
    window.show().unwrap();
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
}
