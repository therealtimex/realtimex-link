include!("../windows_resources.rs");

fn main() {
    // ScreenCaptureKit exists from macOS 12.3; weak-link it so the binary still launches on
    // 12.0-12.2, where screenshots are simply unavailable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }
    windows_resources("RealTimeX Link control");
}
