fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "read_status",
                "set_lending",
                "read_settings",
                "save_settings",
                "set_debug_logging",
                "export_diagnostics",
            ]),
        ))
        .expect("console assets and restricted capabilities must build");
    }
}
