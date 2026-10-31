//! 退出闸：拦下「有长任务在跑」时的退出，先问人，再尽力优雅收尾。
//!
//! 三条铁律（每条都对应一个真实死法，见设计文档 §6.3）：
//! 1. **主线程只做三件事**：`prevent_close()`、CAS 置位、`std::thread::spawn`。
//!    在主线程 `recv_timeout` / `blocking_show` / `sleep` 会让 WebView2 事件循环冻结，
//!    弹窗永远渲染不出来 —— 那不是「卡一下」，是可见的死锁。
//! 2. **先存 sender 再 emit**：反过来的话，前端极快回执会撞上「还没人接收」而白等 3 秒。
//! 3. **ack ≠ 决定**：3 秒超时只判断「前端活没活」；收到 ack 之后无限期等用户决定，
//!    犹豫超过 3 秒的正常用户不该被头上再弹一个原生框。

use crate::i18n;
use crate::process;
use crate::tasks::{TaskRegistry, TaskSnapshot};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 前端 **ack** 窗口：3 秒内收不到「渲染完成回执」= WebView 崩溃 / 冻结 / 监听未注册，
/// 降级原生对话框。注意它检测的是「前端活没活」，**不是**「用户决定没决定」。
pub const FRONTEND_ACK_TIMEOUT: Duration = Duration::from_secs(3);
/// 优雅终止等待（常规退出）：DSH 自拆 + 会话落盘需要时间，3 秒容易掐断写盘。
/// 此时弹窗已确认过用户意图、主窗口已隐藏，多等几秒没有感知成本。
pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(10);

/// 弹窗期间检查「用户是不是又点了一次退出」的心跳间隔。
const RECLICK_POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExitSource {
    Close,
    Tray,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Normal,
    Forced,
}

impl ExitKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Forced => "forced",
        }
    }
}

/// 发给前端弹窗的信号。刻意**不带任务清单**：弹窗一打开就自己调 `get_active_tasks`
/// 并每秒刷新，清单跟着事件走反而会和刷新打架（两个来源，先后顺序不定）。
enum Decision {
    /// 前端「渲染完成」回执（两段式握手的第一段）
    Ack,
    ForceExit,
    KeepRunning,
}

/// `pending`：应答通道。**先存 sender 再 emit**，否则前端的 ack 可能比它就位更快。
/// `reclick`：弹窗期间用户又点了 X / 托盘 —— 「前端在 ack 之后死掉」时这是唯一逃生门。
pub struct ExitGuardState {
    pending: StdMutex<Option<mpsc::Sender<Decision>>>,
    reclick: AtomicBool,
    /// 会话探针的日志节流：每代次只记一条「检测到 N 个会话」或「探针不可用」。
    /// 弹窗 1 Hz 刷新，不节流的话一条日志能刷满整个面板。
    session_logged: AtomicBool,
}

impl ExitGuardState {
    pub fn new() -> Self {
        Self {
            pending: StdMutex::new(None),
            reclick: AtomicBool::new(false),
            session_logged: AtomicBool::new(false),
        }
    }

    /// DSH 重新启动时复位节流（与端点闩锁的复位点相同，见 `process::start_internal`）。
    pub(crate) fn reset_session_log(&self) {
        self.session_logged.store(false, Ordering::SeqCst);
    }
}

impl Default for ExitGuardState {
    fn default() -> Self {
        Self::new()
    }
}

/// 退出请求的统一入口（点 X 的 quit 分支、托盘「退出」都汇到这里）。
///
/// 调用方可能是主线程：所以这里**只**做 CAS 与 spawn，不做任何等待。
pub fn request_exit(app: &AppHandle, source: ExitSource) {
    let registry = app.state::<TaskRegistry>();
    // 先对账：`updating` / `setup_busy` 是「有没有在跑」的权威，快照必须反映此刻
    registry.sync_shell_flags(app);
    // 再刷新会话切片。**顺序不能反**：必须排在取 snapshot() 之前，否则
    // 「明明有会话在跑却静默退出」—— 这正是 S2′ 要消灭的那个行为。
    refresh_session_slice(app);

    if !registry.try_begin_intercept() {
        // 已在弹窗 / 已在退出：不开第二个流程，但记下「又点了一次」。
        // 前端还活着时这只是用户手抖（心跳会先 re-emit 验证，不会误弹原生框）；
        // 前端在 ack 之后死掉时，这是用户唯一的逃生门。
        app.state::<ExitGuardState>()
            .reclick
            .store(true, Ordering::SeqCst);
        return;
    }

    let snapshot = registry.snapshot();
    if snapshot.is_empty() {
        // 需求：无活跃任务 → 静默放行，行为与 v1.5.3 一致。
        // 清理（含最长 10 s 优雅等待）放进 helper 线程：主线程立刻回到事件循环，
        // 窗口不会在退出期间冻结成「(未响应)」幽灵。
        if registry.try_begin_exiting() {
            let app2 = app.clone();
            std::thread::spawn(move || perform_exit(&app2, ExitKind::Normal));
        }
        return;
    }

    // 有活跃任务 → 交互必须发生在独立线程（线程契约见模块文档第 1 条）。
    spawn_interaction(app.clone(), source);
}

fn spawn_interaction(app: AppHandle, source: ExitSource) {
    std::thread::spawn(move || {
        let registry = app.state::<TaskRegistry>();

        // 兜底解锁：这个线程只要没走到「拿到决定」那一步就消失（`blocking_show` 或
        // `emit` 里 panic 都可能），阶段就永远停在 Intercepting —— 之后每次点 X 都只把
        // reclick 置 true 再返回，而能消费 reclick 的线程已经没了，应用从此退不掉。
        // 守卫在「没做决定」时把闸放回 Idle；做了决定就什么都不做（强退那边已是 Exiting）。
        let decided = std::cell::Cell::new(false);
        let _unlock = UnlockOnDrop {
            app: app.clone(),
            decided: &decided,
        };

        // 托盘路径通常发生在窗口隐藏时：先把主窗口唤回来，否则弹窗开在一个看不见的窗口里。
        // 窗口操作必须主线程 —— 这里投递，不直接调。
        if source == ExitSource::Tray {
            crate::show_main_window(&app);
        }

        // 清掉上一轮遗留的逃生门标志（「取消退出 → 再触发」之间的误点会被带进来）
        app.state::<ExitGuardState>()
            .reclick
            .store(false, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel::<Decision>();
        // 先就位 sender，再 emit —— 顺序不能反
        *app.state::<ExitGuardState>().pending.lock().unwrap() = Some(tx);
        // 事件载荷为空：清单由前端自己调 get_active_tasks 拉（见 Decision 的注释）
        let _ = app.emit("exit-confirm-show", ());

        let decision = wait_for_decision(&app, &rx);
        decided.set(true);

        *app.state::<ExitGuardState>().pending.lock().unwrap() = None;
        let _ = app.emit("exit-confirm-hide", ());

        match decision {
            Decision::ForceExit => {
                if registry.try_begin_exiting() {
                    perform_exit(&app, ExitKind::Forced);
                }
                // 失败 = 已经有人在退出（重复点击 / 并发路径），静默返回
            }
            // 其余一律按「不退出」处理。`Ack` 理论上到不了这里（两段式握手只把 ack 当
            // 第一段回执），但真到了也必须是「不退出」—— 退出是不可逆的，宁可多问一次。
            Decision::KeepRunning | Decision::Ack => {
                // 仅当仍是 Intercepting 才回退（系统关机可能已强推进 Exiting）
                registry.cancel_intercept();
                if source == ExitSource::Close {
                    // 取消本次退出 → 藏到托盘，任务继续（与 close_action=tray 的观感一致）
                    let app2 = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        if let Some(w) = process::main_window_handle(&app2) {
                            let _ = w.hide();
                        }
                    });
                }
                // Tray 来源：保持窗口可见（用户本来就是从托盘发起的）
            }
        }
    });
}

/// 交互线程的兜底解锁守卫：线程消失时如果还没拿到决定，就把阶段闸放回 Idle。
/// 没有它的话，交互线程一旦异常退出，`try_begin_intercept` 会永远失败，
/// 用户连点多少次 X 都只是把 `reclick` 置位然后返回 —— 应用再也退不掉。
struct UnlockOnDrop<'a> {
    app: AppHandle,
    decided: &'a std::cell::Cell<bool>,
}

impl Drop for UnlockOnDrop<'_> {
    fn drop(&mut self) {
        if self.decided.get() {
            return;
        }
        self.app.state::<TaskRegistry>().cancel_intercept();
        // 顺带把应答通道摘掉，免得前端后续的 ack 投进一个没人收的 sender
        *self.app.state::<ExitGuardState>().pending.lock().unwrap() = None;
    }
}

/// 两段式握手：3 s 等 ack，之后无限期等决定；每 5 s 检查一次逃生门。
fn wait_for_decision(app: &AppHandle, rx: &mpsc::Receiver<Decision>) -> Decision {
    let registry = app.state::<TaskRegistry>();

    // ---- 第一段：前端活没活 ----
    match rx.recv_timeout(FRONTEND_ACK_TIMEOUT) {
        Ok(Decision::Ack) => {}
        // force/keep 先于 ack 到达（前端没发 ack）：直接采纳
        Ok(d) => return d,
        // 3 s 连 ack 都没收到 = WebView 崩溃 / 页面冻结 / 监听未注册。
        // blocking_show 官方要求不在主线程调用 —— 我们就在这个 helper 线程上弹。
        Err(mpsc::RecvTimeoutError::Timeout) => return native_fallback_dialog(app, &registry),
        // 通道断开（sender 存在 managed state 里，理论上不会）：保守按「不退出」
        Err(mpsc::RecvTimeoutError::Disconnected) => return Decision::KeepRunning,
    }

    // ---- 第二段：用户决定（无限期） ----
    // 用户在弹窗前犹豫多久都不允许再弹第二个原生框 —— 旧稿单段 3 s 超时把
    // 「前端没响应」与「用户还没点」混为一谈，犹豫超过 3 秒就会被头上再弹一个框。
    loop {
        match rx.recv_timeout(RECLICK_POLL) {
            // 重复的 ack（逃生门 re-emit 之后前端会再回一次）不是决定，继续等
            Ok(Decision::Ack) => {}
            Ok(d) => return d,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 逃生门：用户又点了一次退出。先给前端一次「重发」机会 ——
                // 它活着就会再 ack 一次（语义幂等），那只是重弹同一个弹窗；
                // 3 秒仍无回音才判定前端已死，降级原生框。
                if app
                    .state::<ExitGuardState>()
                    .reclick
                    .swap(false, Ordering::SeqCst)
                {
                    let _ = app.emit("exit-confirm-show", ());
                    match rx.recv_timeout(FRONTEND_ACK_TIMEOUT) {
                        Ok(Decision::Ack) => {}
                        Ok(d) => return d,
                        Err(_) => return native_fallback_dialog(app, &registry),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Decision::KeepRunning,
        }
    }
}

fn native_fallback_dialog(app: &AppHandle, registry: &TaskRegistry) -> Decision {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let tasks = registry.snapshot();
    let body = if tasks.is_empty() {
        i18n::t("exit_fallback_none").to_string()
    } else {
        let mut s = i18n::t("exit_fallback_body").to_string();
        for t in tasks.iter().take(5) {
            s.push_str("\n· ");
            s.push_str(&t.title);
        }
        s
    };
    // 按钮顺序与上游一致：Ok = 强制退出、Cancel = 继续后台运行
    let force = app
        .dialog()
        .message(body)
        .title(i18n::t("exit_fallback_title"))
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            i18n::t("exit_btn_force").to_string(),
            i18n::t("exit_btn_keep").to_string(),
        ))
        .blocking_show();
    if force {
        Decision::ForceExit
    } else {
        Decision::KeepRunning
    }
}

/// 清理与退出的唯一入口。
///
/// **线程契约**：恒在非主线程执行（静默放行路径由 `request_exit` spawn，强退路径本就在
/// 交互线程）。因此函数内一切窗口/webview 操作必须经 `run_on_main_thread` 投递；
/// 直接调需要主线程的函数会把布局写坏（旧稿在 helper 线程上调 `persist_before_exit`，
/// 那个函数要求主线程）。
pub fn perform_exit(app: &AppHandle, kind: ExitKind) {
    let registry = app.state::<TaskRegistry>();
    process::log_launcher(app, &i18n::fmt("log_exit_kind", &[&kind.as_str()]));

    // 0) 先隐藏主窗口（best-effort）：下面的优雅等待最长 10 s，用户的感知应该是
    //    「点了退出，窗口立刻没了」，而不是盯着一个不再重绘的窗口。
    //    hide() 不改几何，不影响之后 ExitRequested 里的布局落盘。
    {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = process::main_window_handle(&app2) {
                let _ = w.hide();
            }
        });
    }

    // 1) 案发现场**先落盘**：必须早于杀进程 —— 之后哪怕断电/被 OS 强杀，
    //    磁盘上也已经有这份记录。写失败不阻断退出。
    persist_exit_state(app, kind, registry.snapshot());

    // 2) 优雅终止 → 超时强杀整棵进程树
    process::shutdown_dsh_tree(app, GRACEFUL_TIMEOUT);

    // 3) 退出。布局落盘与 cleanup_sync 交给既有的
    //    RunEvent::ExitRequested / RunEvent::Exit 兜底（它们在主线程，幂等）。
    app.exit(0);
}

// ---------- exit-state.json（案发现场） ----------

/// 一条被中断的任务。只留重放报告需要的字段：`kind` 是稳定枚举，`title` 是当时
/// 本地化的文案（语言可能在下次启动前被改过，如实记录当时看到的那个）。
#[derive(Clone, Serialize, Deserialize)]
pub struct KilledTask {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub started_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct ExitState {
    /// "normal" | "forced"（"system" 由 RunEvent::Exit 兜底写，见 fallback）
    exit_kind: String,
    killed_tasks: Vec<KilledTask>,
    at_ms: u64,
}

/// 启动后前端调一次：有事故记录就返回并**删掉文件**（消费即清，防止每次启动重复报）。
#[derive(Serialize)]
pub struct KilledSessionReport {
    pub exit_kind: String,
    pub tasks: Vec<KilledTask>,
}

fn exit_state_path(app: &AppHandle) -> std::path::PathBuf {
    crate::config::config_dir(app).join("exit-state.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 写案发记录。**正常退出也写一条空列表** —— 把上一轮的旧记录清掉，
/// 否则「上上次」的强退报告会在新会话里复活。
fn persist_exit_state(app: &AppHandle, kind: ExitKind, tasks: Vec<TaskSnapshot>) {
    write_exit_state(app, kind.as_str(), tasks);
}

fn write_exit_state(app: &AppHandle, exit_kind: &str, tasks: Vec<TaskSnapshot>) {
    let state = ExitState {
        exit_kind: exit_kind.to_string(),
        killed_tasks: tasks
            .into_iter()
            .map(|t| KilledTask {
                id: t.id,
                kind: t.kind.as_str().to_string(),
                title: t.title,
                started_at_ms: t.started_at_ms,
            })
            .collect(),
        at_ms: now_ms(),
    };
    let path = exit_state_path(app);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = match serde_json::to_string_pretty(&state) {
        Ok(t) => t,
        Err(e) => {
            // 序列化失败只可能是内部类型问题；退出流程优先，不阻断
            process::log_launcher(app, &i18n::fmt("log_exit_state_fail", &[&e.to_string()]));
            return;
        }
    };
    if let Err(e) = crate::config::write_atomic(&path, text.as_bytes()) {
        process::log_launcher(app, &i18n::fmt("log_exit_state_fail", &[&e.to_string()]));
    }
}

/// 系统关机 / 未经 perform_exit 的退出路径兜底：文件不存在才补写。
/// `perform_exit` 已经写过（正常退出也写空列表）时这里是 no-op；主写因磁盘错误
/// 失败时恰好免费重试一次。
pub fn persist_exit_state_fallback(app: &AppHandle) {
    if exit_state_path(app).exists() {
        return;
    }
    let tasks = app.state::<TaskRegistry>().snapshot();
    write_exit_state(app, "system", tasks);
}

// ---------- 命令三件套 ----------

/// 命令：前端弹窗每秒轮询的活跃任务列表。
#[tauri::command]
pub fn get_active_tasks(app: AppHandle) -> Vec<TaskSnapshot> {
    let registry = app.state::<TaskRegistry>();
    registry.sync_shell_flags(&app);
    // 弹窗每秒刷新靠的就是这一句：任务跑完 → 探针判 Idle → 切片清空 → 行消失
    refresh_session_slice(&app);
    registry.snapshot()
}

/// 刷新 DSH 会话切片（S2′）。`request_exit` 与 `get_active_tasks` 两个入口都调它。
///
/// 只在 DSH 真正在跑时探：`running-external` 不是我们的进程树，安全模式是 3081 的
/// 独立 home（`current_status` 不是 `running` 时直接短路，两条都被挡在门外）。
///
/// 失败语义按设计文档 §15.4：**探针不可用时不造任务、不清切片**（保留上一轮的 stale
/// 结果）—— 宁可多拦一次，也不让已知在跑的任务从清单里静默消失被腰斩。
fn refresh_session_slice(app: &AppHandle) {
    let registry = app.state::<TaskRegistry>();
    if crate::process::current_status(app) != "running" {
        // 权威：DSH 不在了 → 它那些会话当然也都不在了
        registry.sync_session_slice(&[]);
        return;
    }
    let (Some(home), Some(spawned_at)) = (
        crate::process::validated_dsh_home(app),
        crate::process::dsh_started_at(app),
    ) else {
        return; // 拿不到基线就什么都不动（保留上一轮切片）
    };
    match crate::session_probe::probe(
        &home,
        spawned_at,
        std::time::SystemTime::now(),
        crate::session_probe::SESSION_ACTIVE_WINDOW,
    ) {
        crate::session_probe::ProbeOutcome::Active(items) => {
            registry.sync_session_slice(&items);
            // 每个 DSH 代次只记一条：这条是手工验收的唯一依据，但 1 Hz 刷新会让它刷屏，
            // 所以按代次节流。
            //
            // 参数是「N 个会话（工作目录末段 + 命中信号）」而不是光一个 N：排障时要能回答
            // 「凭什么判它活跃」。只写个数字，遇到误判（缓存格式一变就可能）就无从下手。
            let state = app.state::<ExitGuardState>();
            if !state.session_logged.swap(true, Ordering::SeqCst) {
                let detail = items
                    .iter()
                    .map(|it| {
                        let sig: Vec<&str> = it.signals.iter().map(|s| s.as_str()).collect();
                        format!(
                            "{} [{}]",
                            crate::session_probe::cwd_tail(&it.cwd),
                            sig.join("+")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let summary = format!("{} — {}", items.len(), detail);
                process::log_launcher(
                    app,
                    &i18n::fmt("log_session_probe_active", &[&summary]),
                );
            }
        }
        crate::session_probe::ProbeOutcome::Idle => registry.sync_session_slice(&[]),
        crate::session_probe::ProbeOutcome::Unavailable(why) => {
            // 不造任务、不清切片：与 S2 的 Transient 一致
            let state = app.state::<ExitGuardState>();
            if !state.session_logged.swap(true, Ordering::SeqCst) {
                process::log_launcher(
                    app,
                    &i18n::fmt("log_session_probe_unavailable", &[&why]),
                );
            }
        }
    }
}

/// 命令：前端按钮回传用户选择。只投递 channel，无锁等待、无 IO；
/// 重复应答 / 无人在等一律静默成功（前端可能因为 re-emit 而连发两次 ack）。
#[tauri::command]
pub fn exit_decision(app: AppHandle, decision: String) -> Result<(), String> {
    let d = match decision.as_str() {
        "ack" => Decision::Ack,
        "force" => Decision::ForceExit,
        "keep" => Decision::KeepRunning,
        _ => return Err("invalid decision".into()),
    };
    // 用 clone 而不是 take：ack 之后 sender 还要留给逃生门的 re-emit 用。
    // take 的写法（设计文档 §5.5 的骨架）会让第二次 ack 无处投递 —— 前端明明活着、
    // 也照常回了执，交互线程却收不到，3 秒后误判「前端已死」弹原生框。
    let tx = app.state::<ExitGuardState>().pending.lock().unwrap().clone();
    if let Some(tx) = tx {
        let _ = tx.send(d);
    }
    Ok(())
}

/// 命令：启动时读上一次的事故记录（读到即删）。
#[tauri::command]
pub fn get_last_session_killed(app: AppHandle) -> Option<KilledSessionReport> {
    let path = exit_state_path(&app);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let state: ExitState = serde_json::from_str(&text).ok()?;
    // 正常退出（空列表）不是事故：返回 None，前端不会弹提示
    if state.killed_tasks.is_empty() {
        return None;
    }
    Some(KilledSessionReport {
        exit_kind: state.exit_kind,
        tasks: state.killed_tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_kind_labels_are_stable() {
        // 这两个字符串会落进 exit-state.json 并被下次启动读回来，
        // 改了它们等于让老记录读不懂 —— 钉住。
        assert_eq!(ExitKind::Normal.as_str(), "normal");
        assert_eq!(ExitKind::Forced.as_str(), "forced");
    }

    #[test]
    fn exit_state_round_trips() {
        let state = ExitState {
            exit_kind: "forced".to_string(),
            killed_tasks: vec![KilledTask {
                id: "shell-update".to_string(),
                kind: "update".to_string(),
                title: "更新 DSH".to_string(),
                started_at_ms: 1234,
            }],
            at_ms: 5678,
        };
        let text = serde_json::to_string(&state).unwrap();
        let back: ExitState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.exit_kind, "forced");
        assert_eq!(back.killed_tasks.len(), 1);
        assert_eq!(back.killed_tasks[0].id, "shell-update");
        assert_eq!(back.killed_tasks[0].started_at_ms, 1234);
        assert_eq!(back.at_ms, 5678);
    }
}
