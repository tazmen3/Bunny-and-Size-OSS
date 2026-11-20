fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = args.next().ok_or("cache root")?;
    let out = args.next().ok_or("output")?;
    let repetitions = args
        .next()
        .map(|n| n.to_string_lossy().parse::<usize>())
        .transpose()?
        .unwrap_or(30);
    let mode = args.next().unwrap_or_else(|| "auto".into());
    sortsize_slint_spike::qualify_preview(
        root.into(),
        out.into(),
        repetitions,
        mode.to_str()
            .ok_or("memory profile must be a stable ASCII key")?,
    )
}
