fn main() {
    // 前端资源变化时强制重新内嵌（tauri-build 默认不监听 dist 内容变化）
    println!("cargo:rerun-if-changed=../dist");
    // 图标变化时强制重新内嵌
    println!("cargo:rerun-if-changed=../resources/icons");
    tauri_build::build()
}
