fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    if a.len() != 5 {
        return Err("OUTPUT THEME COMPACT WIDTH HEIGHT".into());
    }
    sortsize_slint_spike::i18n_harness::workshop_preview(
        a[0].clone().into(),
        a[1].parse()?,
        a[2].parse()?,
        a[3].parse()?,
        a[4].parse()?,
    )
}
