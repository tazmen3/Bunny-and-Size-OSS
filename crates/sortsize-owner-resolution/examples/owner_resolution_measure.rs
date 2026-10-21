//! Separate, opt-in benchmark. Names/IDs are never emitted. Native modes require
//! an explicit `native-` argument and may contact remote identity providers.
#[path = "support/resolution_alloc.rs"]
mod heap;
use sortsize_core::*;
use sortsize_owner_resolution::*;
use std::{sync::Arc, time::Instant};
struct Fake;
impl OwnerLookup for Fake {
    fn lookup(&self, _: NativeOwnerId, _: LookupLimits) -> OwnerNameResolution {
        OwnerNameResolution::Resolved(AccountName::UnixBytes(
            b"synthetic-account".as_slice().into(),
        ))
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = &args[1];
    let mut count: usize = args[2].parse().unwrap();
    let workers: usize = args.get(3).map_or(2, |s| s.parse().unwrap());
    let overall = Instant::now();
    let scanned = if mode == "scan-native" {
        let mut options = ScanOptions::default();
        options.concurrency.workers = workers;
        Some(
            scan_inventory_with_owners(
                std::path::Path::new(&args[4]),
                &options,
                &CancellationToken::default(),
                InventoryLimits::default(),
                OwnerSelection {
                    owner: true,
                    metadata: MetadataSelection {
                        dates: false,
                        allocation: false,
                    },
                },
                |_| {},
            )
            .unwrap(),
        )
    } else {
        None
    };
    let scan_ms = overall.elapsed().as_secs_f64() * 1000.;
    if let Some(report) = &scanned {
        count = report.owners.len();
    }
    let p = Arc::new(SystemOwnerLookup::new(
        SystemProviderConsent::AllowPotentialNetworkLookup,
    ));
    let lookup: Arc<dyn OwnerLookup> = if mode.contains("native") {
        p.clone()
    } else {
        Arc::new(Fake)
    };
    let config = ResolverConfig {
        workers,
        max_cache_entries: if mode.contains("uncached") { 0 } else { 256 },
        ..Default::default()
    };
    let resolver = OwnerResolver::new(config, lookup).unwrap();
    let base_id = if mode.starts_with("native-") {
        // Fixture acquisition occurs before lookup timing. Only IDs are sent to
        // the resolver; this is NOT a second pass over any user's disk.
        let temp = tempfile::TempDir::new().unwrap();
        let report = scan_inventory_with_owners(
            temp.path(),
            &ScanOptions::default(),
            &CancellationToken::default(),
            InventoryLimits::default(),
            OwnerSelection {
                owner: true,
                metadata: MetadataSelection {
                    dates: false,
                    allocation: false,
                },
            },
            |_| {},
        )
        .unwrap();
        let MetadataValue::Known { value, .. } = report.owners[0].id else {
            panic!("native owner unavailable");
        };
        value
    } else {
        NativeOwnerId::UnixUid(1000)
    };
    let mut latency = Vec::with_capacity(count);
    let (alloc_start, _, _) = heap::snapshot();
    let start = Instant::now();
    for index in 0..count {
        let id = if let Some(report) = &scanned {
            let MetadataValue::Known { value, .. } = report.owners[index].id else {
                panic!("owner unavailable in fixture");
            };
            value
        } else if mode.contains("unique") {
            NativeOwnerId::UnixUid(index as u32)
        } else {
            base_id
        };
        let t = Instant::now();
        match resolver.try_submit(id) {
            SubmitResult::Cached(result) => {
                std::hint::black_box(result);
            }
            SubmitResult::Queued => loop {
                if let Some(result) = resolver.try_take() {
                    std::hint::black_box(result);
                    break;
                }
                std::thread::yield_now();
            },
            other => panic!("unexpected: {other:?}"),
        }
        latency.push(t.elapsed().as_nanos() as u64);
    }
    let seconds = start.elapsed().as_secs_f64();
    let (calls, live, peak) = heap::snapshot();
    let first_ns = latency[0];
    latency.sort_unstable();
    let stats = resolver.stats();
    println!(
        "{}",
        serde_json::json!({
            "mode":mode, "requests":count, "workers":workers, "wall_ms":seconds*1000.,
            "requests_per_second":count as f64/seconds, "scan_ms": scan_ms, "overall_wall_ms": overall.elapsed().as_secs_f64()*1000., "first_ns":first_ns,
            "median_ns":latency[count/2], "p95_ns":latency[(count*95/100).min(count-1)],
            "cache_hits":stats.cache_hits,"cache_misses":stats.cache_misses,"evictions":stats.evictions,
            "lookup_calls":stats.lookup_calls,"native_api_calls":p.native_calls(),
            "cache_entries":stats.cache_entries,"cache_bytes":stats.cache_bytes,
            "fixed_storage_bytes":resolver.fixed_storage_bytes(),
            "allocation_calls_during_requests":calls-alloc_start,"live_requested_bytes":live,"peak_requested_bytes":peak,
            "heap_instrumented":cfg!(feature="owner-heap-profile")
        })
    );
    resolver.cancel();
    while !resolver.is_stopped() {
        std::thread::yield_now();
    }
}
