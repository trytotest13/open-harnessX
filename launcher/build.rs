fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/open-harness.ico");
        res.set("FileDescription", "Open Harness");
        res.set("ProductName", "Open Harness");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        res.compile().expect("embed icon resource");
    }
}
