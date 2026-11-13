fn main() {
    // Build scripts run on the host, so the *target* OS must come from cargo's
    // environment rather than `cfg!`.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource
            .compile()
            .expect("无法编译 Windows 图标资源（需要 rc.exe 或 windres）");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/icon.png");
}
