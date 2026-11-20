//! Real directory acquisition, not a synthetic SQLite benchmark. No recursion.
use sortsize_core::acquisition::{Guard, Policy};
use std::{path::PathBuf, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args_os().nth(1).ok_or("root required")?);
    let start = Instant::now();
    let guard = Guard::open(&root, Policy::default())?;
    let opened = start.elapsed();
    let mut entries = 0u64;
    let mut denied = 0u64;
    let mut logical = 0u64;
    for entry in guard.read_dir(&root)? {
        let entry = entry?;
        entries += 1;
        match guard.inspect(&entry.path()) {
            Ok(s) if s.regular => logical = logical.saturating_add(s.length),
            Ok(_) => {}
            Err(_) => denied += 1,
        }
    }
    println!(
        "{}",
        serde_json::json!({"kind":"native_direct_directory_metadata","synthetic":false,"entries":entries,"denied_or_unavailable":denied,"known_file_logical_bytes":logical,"open_ms":opened.as_secs_f64()*1000.,"elapsed_ms":start.elapsed().as_secs_f64()*1000.,"same_volume":guard.policy().same_volume,"recursive":false,"cache":"OS cache not flushed"})
    );
    Ok(())
}
