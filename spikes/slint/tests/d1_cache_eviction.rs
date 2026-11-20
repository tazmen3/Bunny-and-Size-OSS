//! Regression test for the text dependency lost when Slint evicts a shaping entry.
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::{ComponentHandle, Rgb8Pixel};
use std::rc::Rc;

slint::slint! {
    export component CacheEvictionProbe inherits Window {
        width: 1280px;
        height: 729px;
        background: white;
        in-out property <bool> pulse;
        in-out property <int> fill-count;
        in-out property <string> target-text: "AAAA";
        Rectangle { x: 0px; y: 0px; width: 1280px; height: 650px; background: root.pulse ? #dddddd : #eeeeee; }
        // Visit the target BEFORE the fillers: a sweep during filler rendering
        // must not get hidden by a later target repaint in that same frame.
        Text { x: 20px; y: 700px; width: 1000px; height: 20px; text: root.target-text; color: black; font-size: 12px; }
        for n in root.fill-count: Text {
            x: 10px; y: 10px; width: 300px; height: 20px;
            text: "cache filler"; color: black; font-size: 12px;
        }
    }
}

struct Platform(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Platform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn evicted_shaping_entry_keeps_partial_text_invalidation() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
    let window = CacheEvictionProbe::new().unwrap();
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(1280, 729));
    let mut pixels = vec![Rgb8Pixel::default(); 1280 * 729];
    assert!(adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1280);
    }));
    // Native software surfaces transition from NewBuffer to ReusedBuffer.
    window.set_pulse(true);
    assert!(adapter.draw_if_needed(|r| {
        r.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
        r.render(&mut pixels, 1280);
    }));
    let roi = 1280 * 700..1280 * 720;
    let original = pixels[roi.clone()].to_vec();
    // Two legitimate top-region repaints leave the distant target untouched.
    for pulse in [false, true] {
        window.set_pulse(pulse);
        assert!(adapter.draw_if_needed(|r| {
            r.render(&mut pixels, 1280);
        }));
    }
    // Exceed the 1024-entry shaping cache after the target was last painted.
    window.set_fill_count(1100);
    window.set_pulse(false);
    assert!(adapter.draw_if_needed(|r| {
        r.render(&mut pixels, 1280);
    }));
    window.set_target_text("WWWW".into());
    // The target change ALONE must request a new draw. No unrelated pulse
    // may hide a lost notification after the shaping entry was evicted.
    assert!(
        adapter.draw_if_needed(|r| {
            r.render(&mut pixels, 1280);
            let observed = pixels[roi.clone()].to_vec();
            // Only after recording the incremental frame construct the full oracle.
            let mut reference = vec![Rgb8Pixel::default(); pixels.len()];
            r.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
            r.render(&mut reference, 1280);
            assert_ne!(
                original,
                reference[roi.clone()],
                "oracle must distinguish target text"
            );
            assert_eq!(
                observed, reference[roi],
                "shaping eviction severed text invalidation"
            );
        }),
        "target text change alone must request redraw"
    );
    window.hide().unwrap();
}
