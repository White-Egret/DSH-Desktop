//! 窗口布局记忆：记住主窗口的大小 / 位置 / 最大化状态，下次启动原样恢复。
//!
//! **为什么用官方插件、不自研**：自研「事件防抖 + 定时写 JSON」的失效点不在写入，
//! 而在**恢复** —— 多显示器拔掉之后，磁盘上留下的坐标可能落在已不存在的屏上，
//! 窗口开出来就在可见区域之外，用户唯一的自救手段是删配置文件。
//! `tauri-plugin-window-state` 在恢复时会遍历 `available_monitors()`，只在某个显示器与
//! 「保存的位置 + 尺寸」**相交**时才套用坐标（见其 lib.rs 的 `restore_state`），
//! 这正是那条 Bug 的官方修法。所以本模块只负责「什么时候不该记」，
//! 「怎么记、怎么恢复」全部交给插件，不重复实现。
//!
//! **安全模式的两层隔离**（产品需求：安全模式完全不受窗口布局记忆影响）：
//!
//! 先明确一件事：进入安全模式时，**日常 DSH 实例会被整棵树停掉，再以独立家目录
//! （`%USERPROFILE%\.dsh-safe`、端口 3081）全新启动一个安全实例**（见 safe.rs）——
//! 在"运行环境"这一层，这就是一次干净的重新启动，不是给日常实例换个页面。
//! 但从**桌面外壳**这一层看，本进程与主窗口**不会重启**：始终是同一个 `label="main"`
//! 的窗口，只是把内嵌的 `dsh` webview 指向 3081。
//! 窗口记忆恰恰活在"外壳"这一层，所以上下两层都要有对应处置：
//!
//! 1. **启动期**：以安全模式专用入口启动时（`--safe` / `--safe-mode` / `DSH_SAFE_MODE=1`），
//!    整条插件被 `with_filter(|_| false)` 关掉 —— 既不恢复也不保存，
//!    窗口几何完全由 tauri.conf.json 决定。（这是为「安全模式作为独立入口启动」预留的，
//!    当前的进入按钮不走这条路，见第 2 条。）
//! 2. **点按钮进入/退出安全模式时**：外壳与窗口都不重启，而 `with_filter` 的回调只在
//!    **窗口创建时求值一次**（插件的 `on_window_ready`），运行期等不到第二次机会 ——
//!    所以这条路径必须显式接管，让窗口的表现与"刚按默认配置启动"完全一致：
//!    - 进入安全模式 → 先把当前（日常）布局快照进内存并立刻落盘，再把窗口重置为
//!      tauri.conf.json 里的默认几何；
//!    - 安全模式期间窗口怎么拖、怎么缩，都不再落盘（磁盘上留的是日常布局）；
//!    - 退出安全模式 / 应用退出 → 把内存里的日常布局写回窗口，插件随之把日常布局落盘。
//!
//! 两件事因此同时成立：安全模式看到的永远是「默认初始大小和位置」，而日常模式的窗口布局
//! 不受安全模式期间的任何拖动 / 缩放影响。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Runtime,
    WebviewWindow,
};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

/// 主窗口 label（与 tauri.conf.json、其余模块一致）
const MAIN_WINDOW: &str = "main";

/// 只记这三样：大小、位置、最大化。
///
/// 刻意**不含** `VISIBLE`：本程序「点 X 隐藏到托盘」的路径会让窗口在退出时处于隐藏态，
/// 一旦把可见性也记下来，下次启动就会恢复成「隐藏」，表现为「双击图标没反应」——
/// 正是 show_main_window 里那套 show + unminimize、重绘兜底要救的故障。也不含
/// `DECORATIONS` / `FULLSCREEN`：这两项跟随 tauri.conf.json，不该被历史状态改写。
pub const TRACKED_FLAGS: StateFlags = StateFlags::POSITION
    .union(StateFlags::SIZE)
    .union(StateFlags::MAXIMIZED);

/// 进入安全模式时，是否把主窗口强制重置为 tauri.conf.json 的默认几何。
///
/// 产品要求「安全模式每次进入都用默认初始大小和位置」，故默认开启。
/// 若更希望「进安全模式时窗口保持不动、只是不落盘」，把它改成 `false` 即可 ——
/// 「安全模式不污染日常布局」这条隔离性与此开关无关，两者是独立的两件事。
const RESET_WINDOW_ON_SAFE_ENTER: bool = true;

/// 进程级「当前处于安全模式」标记。
///
/// 存在的唯一原因：`Builder::with_filter` 的回调签名是 `Fn(&str) -> bool`，
/// 只拿得到窗口 label、拿不到 `AppHandle`，没法去读 Tauri 托管的 `SafeState`，
/// 只能读进程级标记。
static SAFE_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 本次会话是否**进入过**安全模式。一旦置位不再清零 ——
/// 退出安全模式后用户仍可能直接退出程序，退出兜底要据此决定是否需要写回日常布局。
static SAFE_MODE_TOUCHED: AtomicBool = AtomicBool::new(false);

/// 日常模式的窗口布局快照（安全模式期间暂存）。
///
/// 用**物理像素**：与插件的落盘格式（2.0.0-rc.4 起 size 存 physical）以及
/// `inner_size` / `outer_position` 的原生返回值一致，避免 DPI 换算误差。
#[derive(Clone, Copy)]
struct Layout {
    size: PhysicalSize<u32>,
    position: Option<PhysicalPosition<i32>>,
    maximized: bool,
}

/// 日常布局的内存快照。安全模式期间它是「日常布局」的唯一可信来源：
/// 插件自己的内存缓存会被安全模式期间的 Moved / Resized 事件改写成安全模式的几何，
/// 而它的 `restore_state` 只认内存缓存、不回读磁盘，所以不能指望插件自己还原。
static DAILY_LAYOUT: Mutex<Option<Layout>> = Mutex::new(None);

// ---------- 启动期安全模式判定 ----------

/// 启动期安全模式判定（纯函数，便于单测）：
/// 环境变量 `DSH_SAFE_MODE` 取真值（`1` / `true` / `yes` / `on`，忽略大小写与首尾空白），
/// 或命令行参数中出现 `--safe` / `--safe-mode`。两者是「或」关系。
fn safe_mode_from(env_value: Option<&str>, args: &[String]) -> bool {
    let env_on = env_value
        .map(|v| {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false);
    env_on
        || args.iter().any(|a| {
            let a = a.trim();
            a == "--safe" || a == "--safe-mode"
        })
}

/// 读取真实启动环境（lib.rs 在 `run()` 里调一次，结果传给 `plugin()`）。
///
/// 注：当前的进入按钮做的是"停掉日常实例 → 用独立家目录重启**安全实例**"，而**桌面外壳
/// 不重启**，所以这里恒为 false；点按钮进出的窗口隔离由下面的显式接管负责。
/// 这个启动期判定是为「安全模式将来作为独立入口、把本程序重新拉起」预留的开关：
/// 真接上 `--safe` 入口时，窗口记忆这边不用再改任何代码。
pub fn is_safe_mode_at_launch() -> bool {
    let env_value = std::env::var("DSH_SAFE_MODE").ok();
    let args: Vec<String> = std::env::args().collect();
    safe_mode_from(env_value.as_deref(), &args)
}

// ---------- 插件 ----------

/// 构造窗口布局记忆插件。
///
/// `launch_safe` 为 true（安全模式专用启动）时 filter 恒为 false：该窗口既不恢复也不保存，
/// 几何完全由 tauri.conf.json 决定。
///
/// 注意 filter 的**语义边界**：它只在窗口创建时被求值一次（插件的 `on_window_ready`），
/// 表达的是「启动期」而非「运行期」；运行期进入安全模式由本模块下面的显式接管处理。
pub fn plugin<R: Runtime>(launch_safe: bool) -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_window_state::Builder::default()
        .with_state_flags(TRACKED_FLAGS)
        .with_filter(move |_label| !launch_safe && !SAFE_MODE_ACTIVE.load(Ordering::SeqCst))
        .build()
}

// ---------- 几何读写 ----------

/// tauri.conf.json 里 `main` 窗口的默认几何（逻辑像素）
struct DefaultGeometry {
    width: f64,
    height: f64,
    center: bool,
    position: Option<(f64, f64)>,
}

/// 取默认几何。这里**读配置而不是写死常量** —— 只要有人改了 tauri.conf.json，
/// 安全模式的重置目标就跟着变，不会出现「代码里写死一个尺寸、配置里已改成别的」的漂移。
/// 配置里找不到 `main` 窗口时返回 None（调用方不动窗口，见文件末尾那条单测）。
fn default_main_geometry<R: Runtime>(app: &AppHandle<R>) -> Option<DefaultGeometry> {
    let win = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_WINDOW)?;
    Some(DefaultGeometry {
        width: win.width,
        height: win.height,
        center: win.center,
        position: match (win.x, win.y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        },
    })
}

/// 抓取当前布局。最小化时 `inner_size` / `outer_position` 返回的是 -32000 之类的哨兵值，
/// 记下来只会在恢复时把窗口丢到屏幕外 —— 这种情况返回 None，让调用方保留旧快照。
fn snapshot<R: Runtime>(window: &WebviewWindow<R>) -> Option<Layout> {
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    // 最大化时 `inner_size` 返回的是整屏大小。先取消最大化，才能拿到真正的
    // 「可还原尺寸」；否则退出安全模式后一旦用户取消最大化，就得到一个满屏窗口。
    if maximized {
        let _ = window.unmaximize();
    }
    Some(Layout {
        size: window.inner_size().ok()?,
        position: window.outer_position().ok(),
        maximized,
    })
}

/// 把布局写回窗口。顺序固定：先取消最大化 → 位置 → 尺寸 → 需要的话再最大化
/// （在最大化状态下设置尺寸是无效操作，所以最大化必须放在最后）。
fn apply<R: Runtime>(window: &WebviewWindow<R>, layout: &Layout) {
    let _ = window.unmaximize();
    if let Some(pos) = layout.position {
        let _ = window.set_position(pos);
    }
    let _ = window.set_size(layout.size);
    if layout.maximized {
        let _ = window.maximize();
    }
}

/// 把主窗口重置为 tauri.conf.json 的默认几何（进入安全模式时执行）
fn reset_to_defaults<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>) {
    let Some(geo) = default_main_geometry(app) else {
        return; // 配置里没有 main 窗口（例如 label 被改名）：不猜、不动窗口
    };
    let _ = window.unmaximize();
    let _ = window.set_size(LogicalSize::new(geo.width, geo.height));
    if geo.center {
        let _ = window.center();
    } else if let Some((x, y)) = geo.position {
        let _ = window.set_position(LogicalPosition::new(x, y));
    }
}

fn daily_layout() -> Option<Layout> {
    *DAILY_LAYOUT.lock().unwrap()
}

/// 把内存里的日常布局写回窗口并立刻落盘。
///
/// 为什么必须显式 `save_window_state`：插件的内存缓存已经被安全模式期间的
/// Moved / Resized 事件改写成安全模式的几何，而它的 `restore_state` 只认缓存、
/// 不回读磁盘 —— 不主动写一次，磁盘上留下的就是安全模式那份。
/// 调用的 `save_window_state` 会先按**当前真实窗口**刷新缓存再序列化，
/// 所以「先 apply 再 save」拿到的就是日常布局。
fn restore_daily_layout<R: Runtime>(app: &AppHandle<R>) {
    let Some(layout) = daily_layout() else {
        return; // 没进过安全模式，或进入时窗口处于最小化（快照被跳过）
    };
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    apply(&window, &layout);
    let _ = app.save_window_state(TRACKED_FLAGS);
}

// ---------- 供 safe.rs / lib.rs 调用的入口 ----------

/// 进入安全模式：冻结日常布局（快照 + 落盘）→ 重置为默认几何。
///
/// 由 safe.rs 的 `enter_safe_blocking` 在**成功 spawn 安全实例之后**调用
/// （失败路径不该动用户的窗口）。内部切主线程：窗口操作必须在主线程执行，
/// 与 lib.rs 的 show_main_window / apply_window_theme 同理。
pub fn on_enter_safe_mode<R: Runtime>(app: &AppHandle<R>) {
    SAFE_MODE_TOUCHED.store(true, Ordering::SeqCst);
    SAFE_MODE_ACTIVE.store(true, Ordering::SeqCst);
    let handle = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(window) = handle.get_webview_window(MAIN_WINDOW) else {
            return;
        };
        if let Some(layout) = snapshot(&window) {
            *DAILY_LAYOUT.lock().unwrap() = Some(layout);
        }
        // 先落盘再改窗口：这样即使安全模式期间程序被强杀，磁盘上留下的也是日常布局，
        // 而不是安全模式的窗口尺寸。
        //
        // 最大化的情况要说明一下：上面的 snapshot 为了拿到「可还原尺寸」已经取消过最大化，
        // 所以这一笔存的是「未最大化的几何 + maximized=false」。走正常路径退出安全模式时，
        // 内存快照里的 maximized=true 会把窗口重新最大化并由下面 restore_daily_layout
        // 再存一次；只有在安全模式里被强杀时才会退化成「下次以正确尺寸但不最大化打开」——
        // 这个方向比「下次开成一个满屏窗口」更安全，故接受。
        let _ = handle.save_window_state(TRACKED_FLAGS);
        if RESET_WINDOW_ON_SAFE_ENTER {
            reset_to_defaults(&handle, &window);
        }
    });
}

/// 退出安全模式：把日常布局写回窗口并落盘。
/// 由 safe.rs 的 `exit_safe_mode` 在停掉安全实例之后、重启日常实例之前调用。
pub fn on_exit_safe_mode<R: Runtime>(app: &AppHandle<R>) {
    SAFE_MODE_ACTIVE.store(false, Ordering::SeqCst);
    let handle = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        restore_daily_layout(&handle);
    });
}

/// 安全模式已结束（含闪退 / 超时停止等非正常路径）：清掉「正在安全模式」标记。
/// `SAFE_MODE_TOUCHED` 不清 —— 退出兜底仍需要知道本次会话进过安全模式。
pub fn mark_safe_mode_inactive() {
    SAFE_MODE_ACTIVE.store(false, Ordering::SeqCst);
}

/// 应用退出前的兜底（lib.rs 在 `RunEvent::ExitRequested` 调用，此时窗口还在）。
/// 本次会话进过安全模式时，磁盘上必须回到日常布局那一份。
///
/// 放在 `ExitRequested` 而不是 `Exit`，是因为 `Exit` 阶段窗口可能已经被销毁，
/// `get_webview_window` 会返回 None 而让兜底静默失效。
/// 这里已在主线程（RunEvent 回调），可直接操作窗口，不需要 run_on_main_thread。
pub fn persist_before_exit<R: Runtime>(app: &AppHandle<R>) {
    if !SAFE_MODE_TOUCHED.load(Ordering::SeqCst) {
        return;
    }
    restore_daily_layout(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_mode_from_env_truthy_values() {
        for v in ["1", "true", "TRUE", " yes ", "On", "on"] {
            assert!(safe_mode_from(Some(v), &[]), "环境变量 {v:?} 应判为安全模式");
        }
    }

    #[test]
    fn safe_mode_from_env_falsy_values() {
        for v in ["", " ", "0", "false", "no", "off", "2", "safe"] {
            assert!(!safe_mode_from(Some(v), &[]), "环境变量 {v:?} 不应判为安全模式");
        }
        assert!(!safe_mode_from(None, &[]), "未设置环境变量时不该判为安全模式");
    }

    #[test]
    fn safe_mode_from_cli_flag() {
        let argv0 = "C:\\Users\\x\\DSH Desktop.exe".to_string();
        let with_safe = vec![argv0.clone(), "--safe".to_string()];
        let with_safe_mode = vec![argv0.clone(), "--safe-mode".to_string()];
        assert!(safe_mode_from(None, &with_safe));
        assert!(safe_mode_from(None, &with_safe_mode));
        // 环境变量与参数是「或」关系：任一路径命中即为安全模式
        assert!(safe_mode_from(Some("0"), &with_safe));
        // 相近但不相同的参数不得误判 —— `--autostart` 是既有参数，必须与安全模式区分开
        assert!(!safe_mode_from(None, &[argv0.clone(), "--autostart".to_string()]));
        assert!(!safe_mode_from(None, &[argv0.clone(), "--safely".to_string()]));
        assert!(!safe_mode_from(None, &[argv0]));
    }

    #[test]
    fn tracked_flags_cover_geometry_only() {
        // 需求：记住大小、位置、最大化
        assert!(TRACKED_FLAGS.contains(StateFlags::SIZE));
        assert!(TRACKED_FLAGS.contains(StateFlags::POSITION));
        assert!(TRACKED_FLAGS.contains(StateFlags::MAXIMIZED));
        // 关键回归：绝不能把可见性也记下来 —— 点 X 隐藏到托盘后退出，
        // 下次启动会恢复成「隐藏」，表现为「点托盘图标打不开窗口」。
        assert!(!TRACKED_FLAGS.contains(StateFlags::VISIBLE));
        // 装饰与全屏跟随 tauri.conf.json，不由历史状态改写
        assert!(!TRACKED_FLAGS.contains(StateFlags::DECORATIONS));
        assert!(!TRACKED_FLAGS.contains(StateFlags::FULLSCREEN));
        // 默认的 StateFlags::default() 是 all，这里必须是显式的子集
        assert_ne!(TRACKED_FLAGS, StateFlags::all());
    }

    /// 默认几何的来源是 tauri.conf.json，而不是代码里的常量。
    /// 这条断言守住前提：配置里必须存在 label="main" 的窗口，且几何是预期的初值 ——
    /// 一旦有人改了 label 或尺寸，这里直接失败，而不是让安全模式的重置悄悄失效
    /// （本机没有 Rust 工具链，这类静默失效在别处发现不了）。
    #[test]
    fn tauri_conf_main_window_geometry() {
        let raw = include_str!("../tauri.conf.json");
        let cfg: serde_json::Value =
            serde_json::from_str(raw).expect("tauri.conf.json 不是合法 JSON");
        let win = cfg["app"]["windows"]
            .as_array()
            .and_then(|list| list.iter().find(|w| w["label"] == MAIN_WINDOW))
            .expect("tauri.conf.json 中缺少 label=\"main\" 的窗口配置");
        assert_eq!(win["width"].as_f64(), Some(1392.0));
        assert_eq!(win["height"].as_f64(), Some(783.0));
        assert_eq!(win["center"].as_bool(), Some(true));
    }
}
