fn main() {
    // Expose the target triple so the app can find `binaries/<tool>-<triple>.exe` in dev.
    println!(
        "cargo:rustc-env=TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("TARGET is set by cargo")
    );
    tauri_build::build();

    // tauri-build links the Windows resource (with the Common Controls v6 manifest)
    // into binaries only. Without it the unit-test exe fails to start with
    // STATUS_ENTRYPOINT_NOT_FOUND, so link the same resource into tests too.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        for name in ["libresource.a", "resource.lib"] {
            let res = out.join(name);
            if res.is_file() {
                println!("cargo:rustc-link-arg-tests={}", res.display());
                println!("cargo:rustc-link-arg-examples={}", res.display());
                break;
            }
        }
    }
}
