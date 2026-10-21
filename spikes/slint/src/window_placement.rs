//! Windows desktop placement only; independent of scanning and retained paths.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn size(self) -> Option<(u32, u32)> {
        let width = u32::try_from(i64::from(self.right) - i64::from(self.left)).ok()?;
        let height = u32::try_from(i64::from(self.bottom) - i64::from(self.top)).ok()?;
        (width > 0 && height > 0).then_some((width, height))
    }
}

#[repr(C)]
#[derive(Default)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowRect(window: *mut c_void, rect: *mut Rect) -> i32;
    fn GetClientRect(window: *mut c_void, rect: *mut Rect) -> i32;
    fn MonitorFromWindow(window: *mut c_void, flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
}

#[derive(Debug, PartialEq, Eq)]
struct Placement {
    position: (i32, i32),
    client: (u32, u32),
}

fn placement(
    outer: Rect,
    client: (u32, u32),
    work: Rect,
    minimum: (u32, u32),
) -> Option<Placement> {
    let (ow, oh) = outer.size()?;
    let (ww, wh) = work.size()?;
    let frame = (ow.checked_sub(client.0)?, oh.checked_sub(client.1)?);
    // Keep the product's 960x600 logical minimum, even on a smaller desktop.
    let width = ow.min(ww).max(minimum.0.checked_add(frame.0)?);
    let height = oh.min(wh).max(minimum.1.checked_add(frame.1)?);
    let left = i64::from(outer.left).clamp(
        i64::from(work.left),
        (i64::from(work.right) - i64::from(width)).max(i64::from(work.left)),
    );
    let top = i64::from(outer.top).clamp(
        i64::from(work.top),
        (i64::from(work.bottom) - i64::from(height)).max(i64::from(work.top)),
    );
    Some(Placement {
        position: (left.try_into().ok()?, top.try_into().ok()?),
        client: (width - frame.0, height - frame.1),
    })
}

pub(super) fn fit_to_work_area(window: &slint::Window) -> bool {
    let owner = window.window_handle();
    let Ok(handle) = owner.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(native) = handle.as_raw() else {
        return false;
    };
    let hwnd = native.hwnd.get() as *mut c_void;
    let (mut outer, mut client) = (Rect::default(), Rect::default());
    let mut info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        ..Default::default()
    };
    // SAFETY: the borrowed HWND remains alive throughout this call. All output
    // buffers have the documented Win32 repr(C) layout and are checked first.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, 2); // MONITOR_DEFAULTTONEAREST
        if monitor.is_null()
            || GetWindowRect(hwnd, &mut outer) == 0
            || GetClientRect(hwnd, &mut client) == 0
            || GetMonitorInfoW(monitor, &mut info) == 0
        {
            return false;
        }
    }
    let scale = window.scale_factor();
    if !scale.is_finite() || scale <= 0.0 {
        return false;
    }
    let minimum = ((960.0 * scale).ceil() as u32, (600.0 * scale).ceil() as u32);
    let Some(client_size) = client.size() else {
        return false;
    };
    let Some(target) = placement(outer, client_size, info.work, minimum) else {
        return false;
    };
    if target.client != client_size {
        window.set_size(slint::PhysicalSize::new(target.client.0, target.client.1));
    }
    if target.position != (outer.left, outer.top) {
        window.set_position(slint::PhysicalPosition::new(
            target.position.0,
            target.position.1,
        ));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_at_200_percent_fits_work_area_including_native_frame() {
        let result = placement(
            Rect {
                left: 147,
                top: 147,
                right: 2573,
                bottom: 1738,
            },
            (2400, 1520),
            Rect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1390,
            },
            (1920, 1200),
        )
        .unwrap();
        assert_eq!(
            result,
            Placement {
                position: (134, 0),
                client: (2400, 1319)
            }
        );
    }

    #[test]
    fn an_existing_visible_window_keeps_its_size_and_position() {
        let result = placement(
            Rect {
                left: 100,
                top: 100,
                right: 1318,
                bottom: 1097,
            },
            (1200, 950),
            Rect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1390,
            },
            (960, 600),
        )
        .unwrap();
        assert_eq!(
            result,
            Placement {
                position: (100, 100),
                client: (1200, 950)
            }
        );
    }

    #[test]
    fn negative_monitors_small_desktops_and_invalid_geometry_are_explicit() {
        let result = placement(
            Rect {
                left: -1000,
                top: -200,
                right: 218,
                bottom: 597,
            },
            (1200, 750),
            Rect {
                left: -1920,
                top: -1080,
                right: -100,
                bottom: -400,
            },
            (960, 600),
        )
        .unwrap();
        assert_eq!(
            result,
            Placement {
                position: (-1318, -1080),
                client: (1200, 633)
            }
        );
        assert_eq!(
            placement(Rect::default(), (0, 0), Rect::default(), (960, 600)),
            None
        );
    }
}
