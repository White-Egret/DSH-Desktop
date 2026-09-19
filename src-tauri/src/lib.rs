mod config;
mod detect;
mod i18n;
mod logger;
mod process;
mod safe;
mod secret;
mod window_state;

use std::time::Duration;
use tauri::{
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItem, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, RunEvent, WindowEvent,
};

/// 启动入口：
/// - `launched_by_autostart` 由 main.rs 检测 `--autostart` 参数后传入。
///   为 true 时窗口保持隐藏（tauri.conf.json visible: false），DSH 由
///   process::start_internal 在延迟 12 秒后静默拉起；用户点托盘图标恢复窗口。
///   为 false（用户手动双击）时 setup 中立即 show 主窗口，行为与旧版一致。
pub fn run(launched_by_autostart: bool) {
    // 启动期只判定一次：安全模式专用入口（`--safe` / `--safe-mode` / `DSH_SAFE_MODE=1`）下
    // 关闭整条窗口布局记忆，让窗口老老实实取 tauri.conf.json 的初始大小与位置。
    // 说明：安全模式本身是"停掉日常实例、用独立家目录重启安全实例"，但**桌面外壳不重启**
    // （同一个进程、同一个 main 窗口），所以点按钮进出时这里恒为 false —— 那条路径的
    // 窗口隔离由 window_state 的显式接管负责。这个开关是为「安全模式将来作为独立入口
    // 重新拉起本程序」预留的，接上后窗口记忆这边不用再改。
    let launch_safe = window_state::is_safe_mode_at_launch();
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
                .app_name("DSH Desktop")
                .build(),
        )
        // 窗口布局记忆（大小 / 位置 / 最大化）：官方 tauri-plugin-window-state。
        // 必须在窗口创建之前注册 —— 插件靠 on_window_ready 钩子做首次恢复。
        // 运行期进安全模式的隔离见 window_state.rs 的模块文档。
        .plugin(window_state::plugin(launch_safe))
        .manage(process::AppState::with_autostart(launched_by_autostart))
        // 安全模式状态（Child 句柄 / 激活标志 / 进入报告）：应用退出与窗口销毁时
        // 由 process::cleanup_sync → safe::cleanup_safe_sync 兜底清理，防孤儿进程占用 3081
        .manage(safe::SafeState::new())
        .invoke_handler(tauri::generate_handler![
            process::get_config,
            process::save_config,
            // 首选项「npm 缓存位置」：同步进 npm 自己的 ~/.npmrc（最小行编辑）+ 读回实际生效值
            process::apply_npm_cache,
            process::npm_cache_info,
            process::get_status,
            process::start_dsh,
            process::stop_dsh,
            process::restart_dsh,
            process::connect_existing,
            process::set_dsh_webview_visible,
            // 工具栏模式（固定显示 / 自动隐藏）：收起状态由前端上报，光标探测兜底
            // 「鼠标回到窗口顶部」这件事（收起时触发条被原生子 webview 盖住）
            process::set_toolbar_hidden,
            process::probe_toolbar_hotzone,
            process::set_toolbar_mode,
            process::refresh_dsh_page,
            process::check_versions,
            process::update_dsh,
            process::detect_npm_package,
            process::pick_exec_path,
            process::pick_folder,
            process::is_autostart_enabled,
            process::set_autostart,
            process::was_launched_by_autostart,
            // 工具命令
            process::check_port,
            process::open_log_dir,
            process::open_in_browser,
            process::detect_environment,
            // 首次运行引导安装
            process::setup_install_node,
            process::setup_install_dsh,
            process::finish_setup,
            process::set_language,
            // 首次运行向导的「Node 版本过低」告警：保留旧版本并继续（只写 node_min_ack 一个键）
            process::remember_node_min_version_notice,
            // 安全模式：独立纯净家目录（%USERPROFILE%\.dsh-safe，端口 3081）
            safe::enter_safe_mode,
            safe::exit_safe_mode,
            safe::get_safe_status,
        ])
        .setup(move |app| {
            // ---- 0. 按用户配置初始化界面语言与外观（后续所有 launcher 日志/托盘菜单文案跟随语言） ----
            let initial_cfg = config::load(app.handle());
            i18n::set_lang(&initial_cfg.language);
            // 原生标题栏深浅与外观设置对齐（页面内部的 data-theme 由 main.js 应用）
            apply_window_theme(app.handle(), &initial_cfg.appearance);

            // ---- 1. 主窗口只在手动启动（非开机自启）时立即显示并聚焦。
            //        关闭拦截统一放在下方 Builder::on_window_event 中处理（hide 而非销毁）。 ----
            if let Some(main) = app.get_webview_window("main") {
                // 缓存主窗口句柄：内嵌 webview 创建后按 label 查找会失效（见 AppState 注释），
                // 热区探测 / 托盘恢复 / 安全模式改标题全指望这份缓存。
                app.state::<process::AppState>().set_main_window(main.clone());
                if !launched_by_autostart {
                    let _ = main.show();
                    let _ = main.set_focus();
                }
                // 预填主窗口尺寸缓存：几何同步的真值来源（见 process::AppState 字段注释）。
                // 必须在首次内嵌 add_child 之前做 —— 之后 get_webview_window("main") 会读不到。
                // 即便这里失败也无妨：第一次 Resized 会用事件自带的窗口句柄补上。
                if let (Ok(scale), Ok(size)) = (main.scale_factor(), main.inner_size()) {
                    let logical: tauri::LogicalSize<f64> = size.to_logical(scale);
                    app.state::<process::AppState>().set_main_window_logical(
                        logical.width,
                        logical.height,
                    );
                }
            }

            // ---- 2. 构建托盘菜单（文案随界面语言） ----
            let show_item = MenuItemBuilder::new(i18n::t("tray_show"))
                .id("show_window")
                .build(app)?;
            let autostart_item = CheckMenuItemBuilder::new(i18n::t("tray_autostart"))
                .id("toggle_autostart")
                .build(app)?;
            // 同步系统实际注册状态到菜单勾选
            {
                use tauri_plugin_autostart::ManagerExt;
                let enabled = app.autolaunch().is_enabled().unwrap_or(false);
                let _ = autostart_item.set_checked(enabled);
            }
            let quit_item = MenuItemBuilder::new(i18n::t("tray_quit"))
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
                show_item: show_item.clone(),
                autostart_item: autostart_item.clone(),
                quit_item: quit_item.clone(),
            });

            // ---- 3. 创建托盘图标（使用打包进二进制的默认窗口图标，已是替换后的鲸鱼图标） ----
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DSH Desktop")
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
                                    let line = i18n::fmt("log_tray_autostart_fail", &[&e.to_string()]);
                                    process::log_launcher(app_handle, &line);
                                }
                                Ok(()) => {
                                    let new_state = !currently;
                                    // 同步托盘菜单勾选
                                    if let Some(items) = app_handle.try_state::<TrayMenuItems>() {
                                        let _ = items.autostart_item.set_checked(new_state);
                                    }
                                    let line = i18n::fmt(
                                        "log_tray_autostart_state",
                                        &[&i18n::t(if new_state { "word_on" } else { "word_off" })],
                                    );
                                    process::log_launcher(app_handle, &line);
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
            // 拦截主窗口关闭：按设置页「点击窗口 X 时」决定隐藏到托盘或退出程序
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                let quit_on_close = {
                    let app = window.app_handle();
                    config::load(app).close_action.trim().eq_ignore_ascii_case("quit")
                };
                if quit_on_close {
                    // 退出程序：结束本次启动的 DSH 进程树（RunEvent::Exit 兜底 cleanup 会执行）
                    window.app_handle().exit(0);
                } else {
                    api.prevent_close();
                    // 直接用事件中的窗口句柄隐藏，避免窗口查找失败导致点 X 无反应
                    let _ = window.hide();
                }
            }
            // 窗口尺寸变化时同步内嵌 DSH Webview 的大小（工具栏 43.2px 之下填满）。
            // 先刷新尺寸缓存：用**事件自带的窗口句柄**读，不查 get_webview_window —
            // 内嵌 webview 创建后那个查找会读不到，而缓存才是几何真值来源
            // （v1.3.2 日志实证：resize 全部退回 1024x640 兜底 → 页面缩小跳左上）。
            WindowEvent::Resized(size) if window.label() == "main" => {
                if let Ok(scale) = window.scale_factor() {
                    let logical: tauri::LogicalSize<f64> = size.to_logical(scale);
                    window
                        .app_handle()
                        .state::<process::AppState>()
                        .set_main_window_logical(logical.width, logical.height);
                }
                process::sync_dsh_webview_size(window.app_handle());
            }
            // 跨显示器拖放导致的 DPI 缩放变化：Windows 不保证一定伴随 Resized，
            // 逻辑尺寸可能因此变化，同样用事件自带的句柄刷新缓存再同步。
            // 注意：WindowEvent 是 non-exhaustive enum，结构体变体模式必须带 `..`（E0638）
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
                ..
            } if window.label() == "main" => {
                let logical: tauri::LogicalSize<f64> = new_inner_size.to_logical(*scale_factor);
                window
                    .app_handle()
                    .state::<process::AppState>()
                    .set_main_window_logical(logical.width, logical.height);
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
            match event {
                // 退出兜底（ExitRequested 早于 Exit 且窗口尚在）：本次会话进过安全模式时，
                // 把日常布局写回窗口并落盘，别让安全模式的窗口尺寸留在 .window-state.json 里。
                // 之所以放在 ExitRequested 而不是 Exit：Exit 阶段窗口可能已被销毁，
                // 那时 get_webview_window 返回 None，兜底会静默失效。
                RunEvent::ExitRequested { .. } => {
                    window_state::persist_before_exit(app);
                    process::cleanup_sync(app);
                }
                // 退出兜底清理：无论正常退出还是异常退出路径，都尝试结束 DSH 进程树
                RunEvent::Exit => {
                    process::cleanup_sync(app);
                }
                _ => {}
            }
        });
}

/// 托盘菜单中需要跨回调访问的菜单项（用于同步勾选状态与语言切换后的文案刷新）
struct TrayMenuItems {
    show_item: MenuItem<tauri::Wry>,
    autostart_item: CheckMenuItem<tauri::Wry>,
    quit_item: MenuItem<tauri::Wry>,
}

/// 语言变更后刷新托盘菜单文字（save_config 时调用；菜单操作需主线程执行）
pub fn refresh_tray_texts(app: &tauri::AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(items) = app.try_state::<TrayMenuItems>() {
            let _ = items.show_item.set_text(i18n::t("tray_show"));
            let _ = items.autostart_item.set_text(i18n::t("tray_autostart"));
            let _ = items.quit_item.set_text(i18n::t("tray_quit"));
        }
    });
}

/// 把外观设置映射到原生窗口主题（标题栏深浅）。
/// light/dark 强制对应主题；system → None 表示跟随操作系统。
/// 这会影响窗口内所有 webview 的 prefers-color-scheme，但内嵌 DSH 页面自身的
/// 深浅由 settings.yaml 的 ui-theme.preference 决定（已在 process::save_config 同步），
/// 两者取值一致，不会打架。
fn theme_for(appearance: &str) -> Option<tauri::Theme> {
    match config::normalize_appearance(appearance) {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None, // system：交还给操作系统决定
    }
}

/// 应用外观到主窗口的原生标题栏（启动时与保存设置时各调用一次）。
pub fn apply_window_theme(app: &tauri::AppHandle, appearance: &str) {
    let theme = theme_for(appearance);
    let app = app.clone();
    // 窗口操作需主线程（与 show_main_window 同理）
    let _ = app.clone().run_on_main_thread(move || {
        // 走缓存句柄：保存外观设置往往发生在内嵌 webview 创建之后，按 label 查找会返回 None
        if let Some(main) = process::main_window_handle(&app) {
            let _ = main.set_theme(theme);
        }
    });
}

fn show_main_window(app: &tauri::AppHandle) {
    // 关键：托盘菜单/图标事件回调运行在后台线程，必须切回主线程才能操作窗口，
    // 否则 show()/set_focus() 会静默失败，表现为“点了托盘无法打开窗口”。
    let app = app.clone();
    // 克隆一份作为 run_on_main_thread 的接收者，闭包内移动的是另一份
    let _ = app.clone().run_on_main_thread(move || {
        // 走缓存句柄：托盘点击往往发生在内嵌 webview 创建之后，按 label 查找会返回 None
        if let Some(w) = process::main_window_handle(&app) {
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
        } else if let Some(win) = app.get_window("main") {
            // 兜底：webview window 查找失败时直接操作原生窗口
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    });
}