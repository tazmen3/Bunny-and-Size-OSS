//! Local diagnostic only; no file scan and no network unless explicitly enabled.
#[cfg(any(target_os = "linux", windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use sortsize_core::{MetadataValue, NativePath, volumes::*};
    let remote = std::env::args().any(|s| s == "--probe-remote");
    let r = discover_volumes(DiscoveryOptions {
        probe_remote: remote,
        ..Default::default()
    })?;
    let numeric = |v: MetadataValue<u64>| match v {
        MetadataValue::Known { value, .. } => Some(value),
        _ => None,
    };
    let rows:Vec<_>=r.volumes.iter().map(|v| serde_json::json!({
        "identity":format!("{:?}",v.identity),"kind":format!("{:?}",v.kind),"availability":format!("{:?}",v.availability),
        "mounts":v.mounts.iter().map(|m|NativePath::from(m.path.as_path())).collect::<Vec<_>>(),
        "total":numeric(v.capacity.total),"free_total":numeric(v.capacity.free_total),"available":numeric(v.capacity.available),"used":numeric(v.capacity.used),"caller_total":numeric(v.capacity.caller_total)
    })).collect();
    println!(
        "{}",
        serde_json::json!({"enumeration_ms":r.enumeration_time.as_secs_f64()*1000.0,"capacity_ms":r.capacity_time.as_secs_f64()*1000.0,"capacity_calls":r.capacity_calls,"volumes":rows})
    );
    Ok(())
}
#[cfg(not(any(target_os = "linux", windows)))]
fn main() {}
