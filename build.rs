// build.rs
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winresource::WindowsResource::new();
        // 设置图标
        res.set_icon("app.ico");

        res.compile().unwrap();
    }
}
