mod version_resources {
    include!("../../packaging/windows/version-resource.rs");

    pub fn compile() {
        main();
    }
}

fn main() {
    version_resources::compile();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Slint's generated root view constructor needs more than the default
        // Windows main-thread stack in debug builds. Reserve address space
        // while retaining the linker's default initial commit.
        println!("cargo:rustc-link-arg-bin=bunny-and-size=/STACK:8388608");
    }
    // Compile the isolated pilot first: compile_with_config sets the generated
    // include path. The final compile below must remain the production UI.
    if std::env::var_os("CARGO_FEATURE_I18N_PILOT").is_some() {
        let config = slint_build::CompilerConfiguration::new()
            .with_bundled_translations("tests/fixtures/i18n/lang");
        slint_build::compile_with_config("tests/fixtures/i18n/i18n_pilot.slint", config)
            .expect("compile isolated i18n pilot");
    }
    // Pseudolocale is a laboratory-only bundle. Product builds keep exactly
    // the seven user languages and never persist a pseudo language preference.
    let translations = if std::env::var_os("CARGO_FEATURE_BENCHMARKS").is_some() {
        let root = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("l16-lang");
        for lang in ["fr", "de", "es", "pt", "it", "nl"] {
            let target = root.join(lang).join("LC_MESSAGES");
            std::fs::create_dir_all(&target).unwrap();
            let source = format!("lang/{lang}/LC_MESSAGES/sortsize-slint-spike.po");
            println!("cargo:rerun-if-changed={source}");
            std::fs::copy(source, target.join("sortsize-slint-spike.po")).unwrap();
        }
        let target = root.join("qps-ploc/LC_MESSAGES");
        std::fs::create_dir_all(&target).unwrap();
        println!("cargo:rerun-if-changed=tests/fixtures/i18n/l16-pseudo.po");
        std::fs::copy(
            "tests/fixtures/i18n/l16-pseudo.po",
            target.join("sortsize-slint-spike.po"),
        )
        .unwrap();
        root
    } else {
        std::path::PathBuf::from("lang")
    };
    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new()
            .with_bundled_translations(translations.to_str().unwrap()),
    )
    .expect("compile Slint UI with product translations");
}
