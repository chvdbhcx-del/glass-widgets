// Embeds the app icon into the exe (Explorer, taskbar, Add/Remove Programs). A missing resource compiler only skips the icon.
fn main() {
    println!("cargo:rerun-if-changed=installer/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("installer/icon.ico");
        res.set("ProductName", "Glass Widgets");
        res.set("FileDescription", "Glass Widgets");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
