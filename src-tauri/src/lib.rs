mod config;
mod process;

use tauri::{Manager, RunEvent, WindowEvent};

pub fn run() {
    tauri::Builder::default()
        // 单实例锁必须最先注册：第二次启动时聚焦已有窗口，而不是再启动一个 DSH
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(process::AppState::new())
        .invoke_handler(tauri::generate_handler![
            process::get_config,
            process::save_config,
            process::get_status,
            process::start_dsh,
            process::stop_dsh,
            process::restart_dsh,
            process::connect_existing,
            process::set_dsh_webview_visible,
            process::check_versions,
            process::update_dsh,
            process::detect_npm_package,
            process::pick_exec_path,
            process::pick_folder,
        ])
        .on_window_event(|window, event| match event {
            // 窗口尺寸变化时同步内嵌 DSH Webview 的大小（工具栏 48px 之下填满）
            WindowEvent::Resized(_) if window.label() == "main" => {
                process::sync_dsh_webview_size(window.app_handle());
            }
            // 关闭主窗口 = 退出整个程序：停止 DSH（内嵌 webview 随窗口一起销毁）
            WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed if window.label() == "main" => {
                process::cleanup_sync(window.app_handle());
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application")
        .run(|app, event| {
            // 退出兜底清理：无论正常退出还是异常退出路径，都尝试结束 DSH 进程树
            if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
                process::cleanup_sync(app);
            }
        });
}
