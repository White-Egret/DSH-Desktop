mod config;
mod process;

use std::time::Duration;
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, RunEvent, WindowEvent,
};

/// 启动入口：
/// - `launched_by_autostart` 由 main.rs 检测 `--autostart` 参数后传入。
///   为 true 时窗口保持隐藏（tauri.conf.json visible: false），DSH 由
///   process::start_internal 在延迟 12 秒后静默拉起；用户点托盘图标恢复窗口。
///   为 false（用户手动双击）时 setup 中立即 show 主窗口，行为与旧版一致。
pub fn run(launched_by_autostart: bool) {
    tauri::Builder::default()
        // 单实例锁必须最先注册：第二次启动时聚焦已有窗口，而不是再启动一个 DSH
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 单例二次唤醒：隐藏的最小化窗口同样要 show + unminimize + set_focus
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        // 官方开机自启插件：注册时传 `--autostart` 参数，启动时由 main.rs 检测
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--autostart"])
                .app_name("DSH Launcher")
                .build(),
        )
        .manage(process::AppState::with_autostart(launched_by_autostart))
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
            process::is_autostart_enabled,
            process::set_autostart,
            process::was_launched_by_autostart,
        ])
        .setup(move |app| {
            // ---- 1. 主窗口只在手动启动（非开机自启）时立即显示并聚焦。
            //        关闭拦截统一放在下方 Builder::on_window_event 中处理（hide 而非销毁）。 ----
            if let Some(main) = app.get_webview_window("main") {
                if !launched_by_autostart {
                    let _ = main.show();
                    let _ = main.set_focus();
                }
            }

            // ---- 2. 构建托盘菜单 ----
            let show_item = MenuItemBuilder::new("显示主窗口")
                .id("show_window")
                .build(app)?;
            let autostart_item = CheckMenuItemBuilder::new("开机自动启动")
                .id("toggle_autostart")
                .build(app)?;
            // 同步系统实际注册状态到菜单勾选
            {
                use tauri_plugin_autostart::ManagerExt;
                let enabled = app.autolaunch().is_enabled().unwrap_or(false);
                let _ = autostart_item.set_checked(enabled);
            }
            let quit_item = MenuItemBuilder::new("退出 Launcher")
                .id("quit_app")
                .build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&show_item)
                .item(&autostart_item)
                .separator()
                .item(&quit_item)
                .build()?;

            // 将菜单项放入 managed state，托盘菜单回调和前端 invoke 都能同步勾选
            app.manage(TrayMenuItems {
                autostart_item: autostart_item.clone(),
            });

            // ---- 3. 创建托盘图标（使用打包进二进制的默认窗口图标，已是替换后的鲸鱼图标） ----
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DSH Launcher")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|tray, event| {
                    let app_handle = tray.app_handle();
                    match event.id().as_ref() {
                        "show_window" => {
                            show_main_window(app_handle);
                        }
                        "toggle_autostart" => {
                            use tauri_plugin_autostart::ManagerExt;
                            let mgr = app_handle.autolaunch();
                            let currently = mgr.is_enabled().unwrap_or(false);
                            let result = if currently {
                                mgr.disable()
                            } else {
                                mgr.enable()
                            };
                            match result {
                                Err(e) => {
                                    let line = format!("[launcher] 切换开机自启失败：{}", e);
                                    let _ = app_handle.emit(
                                        "dsh-log",
                                        process::LogEvent {
                                            stream: "launcher".to_string(),
                                            line,
                                        },
                                    );
                                }
                                Ok(()) => {
                                    let new_state = !currently;
                                    // 同步托盘菜单勾选
                                    if let Some(items) = app_handle.try_state::<TrayMenuItems>() {
                                        let _ = items.autostart_item.set_checked(new_state);
                                    }
                                    let line = format!(
                                        "[launcher] 开机自启已{}。",
                                        if new_state { "开启" } else { "关闭" }
                                    );
                                    let _ = app_handle.emit(
                                        "dsh-log",
                                        process::LogEvent {
                                            stream: "launcher".to_string(),
                                            line,
                                        },
                                    );
                                    // 通知前端同步设置开关
                                    let _ = app_handle.emit(
                                        "autostart-changed",
                                        process::LogEvent {
                                            stream: "launcher".to_string(),
                                            line: if new_state {
                                                "on".to_string()
                                            } else {
                                                "off".to_string()
                                            },
                                        },
                                    );
                                }
                            }
                        }
                        "quit_app" => {
                            app_handle.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击/双击 → 直接显示主窗口（show_menu_on_left_click = false）
                    match event {
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } => {
                            show_main_window(tray.app_handle());
                        }
                        TrayIconEvent::DoubleClick { .. } => {
                            show_main_window(tray.app_handle());
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 拦截主窗口关闭：阻止销毁，改为隐藏到托盘（X 不再退出程序）
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                api.prevent_close();
                // 直接用事件中的窗口句柄隐藏，避免窗口查找失败导致点 X 无反应
                let _ = window.hide();
            }
            // 窗口尺寸变化时同步内嵌 DSH Webview 的大小（工具栏 43.2px 之下填满）
            WindowEvent::Resized(_) if window.label() == "main" => {
                process::sync_dsh_webview_size(window.app_handle());
            }
            // 真正销毁时清理 DSH 进程（CloseRequested 已被拦截转 hide，正常路径不会到这里）
            WindowEvent::Destroyed if window.label() == "main" => {
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

/// 托盘菜单中需要跨回调访问的菜单项（用于同步勾选状态）
struct TrayMenuItems {
    autostart_item: CheckMenuItem<tauri::Wry>,
}

fn show_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();

        // Windows WebView2 在 hide() 后再 show() 偶尔白屏/假死：
        // 用宽度 +1/-1 的微小 resize 强制触发 WebView 重绘。
        if let Ok(size) = w.inner_size() {
            let _ = w.set_size(tauri::PhysicalSize::new(size.width + 1, size.height));
            std::thread::sleep(Duration::from_millis(10));
            let _ = w.set_size(size);
        }
        // 尺寸抖动后再次确认焦点（Windows 焦点抢占需要最后再执行一次）
        let _ = w.set_focus();
    }
}