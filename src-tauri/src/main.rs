// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 检测是否由「开机自启」触发（Windows 注册表 Run 项会带这个参数）。
    // 若带 → lib.rs 中 tauri.conf.json 的 visible:false 让窗口保持隐藏，
    //       process::start_internal 会在拉起 DSH 前 sleep 12 秒错开系统冷启动高峰。
    let launched_by_autostart = std::env::args().any(|a| a == "--autostart");
    dsh_launcher_lib::run(launched_by_autostart);
}