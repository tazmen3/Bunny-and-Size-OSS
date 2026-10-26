fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 3 {
        return Err("Usage: sortsize-i18n-harness en|fr|b2-en|b2-fr|c-en|c-fr|c-de|c-es|c-pt FIXTURE OUTPUT".into());
    }
    let mode = arguments[0].to_str().ok_or("invalid mode")?;
    let b2 = mode.starts_with("b2-") || mode.starts_with("c-");
    let language = match mode.trim_start_matches("b2-").trim_start_matches("c-") {
        "en" => sortsize_slint_spike::locale::Language::English,
        "fr" => sortsize_slint_spike::locale::Language::French,
        "de" if b2 => sortsize_slint_spike::locale::Language::German,
        "es" if b2 => sortsize_slint_spike::locale::Language::Spanish,
        "pt" if b2 => sortsize_slint_spike::locale::Language::Portuguese,
        _ => return Err("Unsupported harness language/mode".into()),
    };
    let run = if b2 {
        sortsize_slint_spike::i18n_harness::run_b2
    } else {
        sortsize_slint_spike::i18n_harness::run
    };
    run(
        language,
        arguments[1].clone().into(),
        arguments[2].clone().into(),
    )
}
