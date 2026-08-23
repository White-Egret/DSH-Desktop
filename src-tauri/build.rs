fn main() {
    // 图标/配置变化时强制重新生成 Windows 资源并重新链接，
    // 避免 CI 的 target 缓存复用到旧版本嵌入的 exe 图标（桌面/任务栏图标不更新的根源）
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    println!("cargo:rerun-if-changed=icons/32x32.png");
    println!("cargo:rerun-if-changed=icons/128x128.png");
    println!("cargo:rerun-if-changed=icons/128x128@2x.png");
    tauri_build::build()
}
