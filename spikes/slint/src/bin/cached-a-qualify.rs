fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() == 2 && args[0] == "--prepare-smoke" {
        return sortsize_slint_spike::prepare_cached_a_smoke(args[1].clone().into());
    }
    if args.len() != 4 {
        return Err("REPOSITORY FIXTURE_JSON MODE(auto|50000|250000|1000000|5000000|unlimited-count) OUTPUT_DIR".into());
    }
    sortsize_slint_spike::qualify_cached_a(
        args[0].clone().into(),
        args[1].clone().into(),
        args[2].to_str().ok_or("ASCII memory profile required")?,
        args[3].clone().into(),
    )
}
