fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // MSI must compare executable versions rather than treating a file
        // restored by rollback as modified user data and preserving old code.
        tauri_winres::WindowsResource::new()
            .compile()
            .expect("Windows executables must carry the Cargo package version");
    }
}
