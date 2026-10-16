#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> Result<(), slint::PlatformError> {
    sortsize_slint_spike::run()
}
