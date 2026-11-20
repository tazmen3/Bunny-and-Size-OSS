fn main() -> Result<(), Box<dyn std::error::Error>> {
    sortsize_slint_spike::qualify_finish(
        std::env::args_os()
            .nth(1)
            .ok_or("output directory required")?
            .into(),
    )
}
