fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/open-harness.ico");
        res.set("FileDescription", "Open Harness");
        res.set("ProductName", "Open Harness");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        if let Err(e) = res.compile() {
            // Machines without the Windows SDK have no rc.exe. Link the
            // resource object from a previous build so the icon and version
            // info survive instead of failing the whole compile.
            let cached = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/open-harness.res");
            if cached.is_file() {
                println!(
                    "cargo:warning=resource compiler unavailable ({e}); linking {}",
                    cached.display()
                );
                println!("cargo:rustc-link-arg={}", cached.display());
            } else {
                println!("cargo:warning=resource compiler unavailable ({e}); building without icon resources");
            }
        }
    }
}
