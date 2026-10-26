mod version_resources {
    include!("../../packaging/windows/version-resource.rs");

    pub fn compile() {
        main();
    }
}

fn main() {
    version_resources::compile();
    // Compile the isolated pilot first: compile_with_config sets the generated
    // include path. The final compile below must remain the production UI.
    if std::env::var_os("CARGO_FEATURE_I18N_PILOT").is_some() {
        let config = slint_build::CompilerConfiguration::new()
            .with_bundled_translations("tests/fixtures/i18n/lang");
        slint_build::compile_with_config("tests/fixtures/i18n/i18n_pilot.slint", config)
            .expect("compile isolated i18n pilot");
    }
    slint_build::compile_with_config(
        "ui/main.slint",
        slint_build::CompilerConfiguration::new().with_bundled_translations("lang"),
    )
    .expect("compile Slint UI with product translations");
}
