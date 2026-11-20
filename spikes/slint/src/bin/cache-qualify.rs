fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .ok_or("OUTPUT directory required")?;
    sortsize_slint_spike::qualify_cache(out.into())
}
