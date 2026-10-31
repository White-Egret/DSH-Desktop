//! 活跃任务注册表 + 全局退出锁。
//!
//! 本期只登记**壳内真实存在的**两类长任务：`更新 DSH`（npm，可能数分钟）与
//! `引导安装`（node / dsh / pnpm / Python）。DSH 进程内部的 Agent 任务要等 DSH 侧
//! 暴露任务端点之后才可见，不在本期范围 —— 这里刻意不预留任何空分支，
//! 没有消费者的接口写出来只会变成永远返回空、没人验证得动的死代码。
//!
//! 并发选型（论证见设计文档 §8）：`AtomicU8` 阶段闸 + `std::sync::Mutex<BTreeMap>`，
//! 不引入 tokio —— 本 crate 全部后台流都是 std::thread，临界区内没有 await 点。

use crate::i18n;
use crate::process::AppState;
use crate::session_probe::SessionActivity;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_INTERCEPTING: u8 = 1;
pub const PHASE_EXITING: u8 = 2;

/// 壳内长任务的类别。前端按它取 `exit_task_kind_*` 文案（而不是直接用 Rust 传来的
/// title），这样弹窗开着时切换语言也能跟着变。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Update,
    Install,
    /// DSH 内部的 Agent 会话（S2′ 会话探针从本地缓存读出；不是壳内任务）
    DshSession,
}

impl TaskKind {
    fn i18n_key(self) -> &'static str {
        match self {
            Self::Update => "exit_task_kind_update",
            Self::Install => "exit_task_kind_install",
            Self::DshSession => "exit_task_kind_dsh_session",
        }
    }

    /// 落进 `exit-state.json` 的稳定标识（与 Serialize 的 snake_case 一致）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Install => "install",
            Self::DshSession => "dsh_session",
        }
    }
}

#[derive(Clone, Serialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub kind: TaskKind,
    /// 已本地化的任务标题（给**原生降级对话框**用；前端弹窗按 kind 自己取文案）
    pub title: String,
    pub started_at_ms: u64,
}

/// 壳内任务在表里的固定键。用固定键而不是自增 id，是因为这两个任务的权威来源就是
/// `AppState` 里的两个 AtomicBool：同一时刻只可能各有一个，键天然稳定。
const ID_UPDATE: &str = "shell-update";
const ID_INSTALL: &str = "shell-install";

/// DSH 会话任务在表里的键前缀。S2′ 用 `dsh:<session-id>`，S2 的端点落地后沿用同一片
/// 切片（键跨刷新稳定 → 列表不跳动、计时真实）。
const ID_DSH_PREFIX: &str = "dsh:";

pub struct TaskRegistry {
    phase: AtomicU8,
    /// BTreeMap：快照按 id 稳定排序，前端列表不会因为 HashMap 的迭代顺序而跳动
    tasks: Mutex<BTreeMap<String, TaskSnapshot>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_IDLE),
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }

    // ---- 阶段闸：全局退出锁的核心。全部 CAS，**绝不碰 tasks 锁** ----
    //
    // 不碰 tasks 锁是刻意的：阶段判定与任务数据访问解耦之后，退出流程的任何一条路径
    // 都不会因为「某个线程正抱着任务表等 IO」而卡住 —— 退出路径最怕的就是等。

    /// Idle → Intercepting。X 与托盘并发点击时只有一个能进来。
    pub fn try_begin_intercept(&self) -> bool {
        self.phase
            .compare_exchange(
                PHASE_IDLE,
                PHASE_INTERCEPTING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Intercepting → Idle（用户选了「继续后台运行」）。只在仍是 Intercepting 时回退：
    /// 系统关机可能已经把阶段强推进 Exiting，那时不能把终态拉回来。
    pub fn cancel_intercept(&self) {
        let _ = self.phase.compare_exchange(
            PHASE_INTERCEPTING,
            PHASE_IDLE,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Idle / Intercepting → Exiting（终态）。返回 true 的一方负责执行 `perform_exit`，
    /// 因此强退与系统关机并发时天然只有一个执行者，`perform_exit` 不会被跑两遍。
    pub fn try_begin_exiting(&self) -> bool {
        self.phase
            .compare_exchange(
                PHASE_INTERCEPTING,
                PHASE_EXITING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
            || self
                .phase
                .compare_exchange(
                    PHASE_IDLE,
                    PHASE_EXITING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
    }

    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        self.tasks.lock().unwrap().values().cloned().collect()
    }

    /// 用既有原子标志**对账**，而不是在 `update_dsh` / `setup_install_*` 的十几个
    /// `updating.store(false)` 出口逐点插 register/finish —— 漏一处就会留下一个永不消失
    /// 的任务，弹窗从此每次退出都出现。
    ///
    /// - `updating` / `setup_busy` 是「有没有在跑」的权威（既有代码已保证所有出口都复位）
    /// - 只读 AtomicBool，不拿 AppState 的 Mutex，因此不引入锁序问题
    /// - `started_at` 由第一次观察到 true 时记下，之后保持不变（弹窗里的「已运行 N 秒」才是真的）
    pub fn sync_shell_flags(&self, app: &AppHandle) {
        let state = app.state::<AppState>();
        self.reconcile(
            state.updating.load(Ordering::SeqCst),
            state.setup_busy.load(Ordering::SeqCst),
            now_ms(),
        );
    }

    /// `sync_shell_flags` 的可测内核：只吃两个布尔与一个时间戳，不碰 AppHandle。
    fn reconcile(&self, updating: bool, setup_busy: bool, now: u64) {
        // Exiting 阶段不再新增条目：此刻 perform_exit 已经快照完并开始杀进程，
        // 这时候冒出来的「新任务」只会让案发记录与实际不符。已有条目照常删除。
        let may_insert = self.phase() != PHASE_EXITING;
        let mut map = self.tasks.lock().unwrap();
        for (id, running, kind) in [
            (ID_UPDATE, updating, TaskKind::Update),
            (ID_INSTALL, setup_busy, TaskKind::Install),
        ] {
            if running {
                if may_insert && !map.contains_key(id) {
                    map.insert(
                        id.to_string(),
                        TaskSnapshot {
                            id: id.to_string(),
                            kind,
                            title: i18n::t(kind.i18n_key()).to_string(),
                            started_at_ms: now,
                        },
                    );
                }
            } else {
                map.remove(id);
            }
        }
    }

    /// DSH 会话切片的整片替换（S2′ 会话探针的数据出口）。
    ///
    /// 语义与 §4 规划的 `replace_dsh_slice` 一致：先删掉所有 `dsh:` 前缀的旧条目，再按
    /// 传入的列表重建。键用 `dsh:<session-id>`，跨刷新稳定 —— 同一会话在两轮探测之间
    /// id 不变，`started_at_ms` 由**首次观察**记下，所以弹窗里的秒数是真的在走而不是
    /// 每次刷新都归零。
    ///
    /// 传空切片 = 权威地「没有活跃会话」（DSH 不在跑 / 探针判 Idle），会清空这一片。
    /// 探针**不可用**时不要调它 —— 那种情况要保留上一轮切片（stale），宁可多拦一次。
    pub(crate) fn sync_session_slice(&self, items: &[SessionActivity]) {
        let may_insert = self.phase() != PHASE_EXITING;
        let now = now_ms();
        let mut map = self.tasks.lock().unwrap();
        // 先记住旧条目的 started_at：已有会话必须沿用第一次看到它的时刻
        let prev: BTreeMap<String, u64> = map
            .iter()
            .filter(|(k, _)| k.starts_with(ID_DSH_PREFIX))
            .map(|(k, v)| (k.clone(), v.started_at_ms))
            .collect();
        map.retain(|k, _| !k.starts_with(ID_DSH_PREFIX));
        if !may_insert {
            return; // Exiting：只删不加
        }
        for item in items {
            let id = format!("{ID_DSH_PREFIX}{}", item.session_id);
            let started_at_ms = prev.get(&id).copied().unwrap_or(now);
            let tail = crate::session_probe::cwd_tail(&item.cwd);
            // 标题带上工作目录末段：弹窗里两三个会话并列时，光「DSH 会话」分不出是哪个
            let title = if tail.is_empty() {
                i18n::t("exit_task_kind_dsh_session").to_string()
            } else {
                i18n::fmt("exit_task_dsh_session_title", &[&tail])
            };
            map.insert(
                id.clone(),
                TaskSnapshot {
                    id,
                    kind: TaskKind::DshSession,
                    title,
                    started_at_ms,
                },
            );
        }
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// N 个线程同时抢同一个阶段闸：恰好一个成功。这是「连点 X ×3 + 立刻点托盘退出
    /// 只走一条流程」的全部依据 —— 它一旦不成立，就会出现两个 perform_exit 同时杀进程。
    #[test]
    fn only_one_thread_enters_intercept() {
        for _ in 0..50 {
            let reg = Arc::new(TaskRegistry::new());
            let mut handles = Vec::new();
            for _ in 0..8 {
                let r = reg.clone();
                handles.push(std::thread::spawn(move || r.try_begin_intercept()));
            }
            let winners = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|ok| *ok)
                .count();
            assert_eq!(winners, 1, "try_begin_intercept 必须恰好放一个进来");
        }
    }

    #[test]
    fn only_one_thread_begins_exiting() {
        for _ in 0..50 {
            let reg = Arc::new(TaskRegistry::new());
            let mut handles = Vec::new();
            for _ in 0..8 {
                let r = reg.clone();
                handles.push(std::thread::spawn(move || r.try_begin_exiting()));
            }
            let winners = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|ok| *ok)
                .count();
            assert_eq!(winners, 1, "perform_exit 只能有一个执行者");
            assert_eq!(reg.phase(), PHASE_EXITING);
        }
    }

    /// 系统关机与用户强退并发：两者都可能调 try_begin_exiting，但只有一个赢。
    #[test]
    fn exiting_from_intercepting_and_idle_still_yields_one_winner() {
        let reg = TaskRegistry::new();
        assert!(reg.try_begin_intercept());
        let winners = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4).map(|_| s.spawn(|| reg.try_begin_exiting())).collect();
            let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            results.into_iter().filter(|ok| *ok).count()
        });
        assert_eq!(winners, 1);
    }

    #[test]
    fn cancel_intercept_allows_entering_again() {
        let reg = TaskRegistry::new();
        assert!(reg.try_begin_intercept());
        assert!(!reg.try_begin_intercept(), "弹窗期间第二次点击必须被吞掉");
        reg.cancel_intercept();
        assert_eq!(reg.phase(), PHASE_IDLE);
        assert!(reg.try_begin_intercept(), "取消后必须能再次进入");
    }

    #[test]
    fn cancel_intercept_does_not_pull_back_from_exiting() {
        let reg = TaskRegistry::new();
        assert!(reg.try_begin_exiting());
        reg.cancel_intercept();
        assert_eq!(reg.phase(), PHASE_EXITING, "Exiting 是终态，不能被拉回");
    }

    #[test]
    fn reconcile_inserts_and_removes() {
        let reg = TaskRegistry::new();
        reg.reconcile(true, false, 1000);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "shell-update");
        assert_eq!(snap[0].kind, TaskKind::Update);
        assert_eq!(snap[0].started_at_ms, 1000);

        // 两个都跑起来：列表按 id 稳定排序（shell-install < shell-update）
        reg.reconcile(true, true, 2000);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, "shell-install");
        assert_eq!(snap[1].id, "shell-update");

        // 更新跑完了：只剩安装，且它的开始时间没有被后来的观察重置
        reg.reconcile(false, true, 3000);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "shell-install");
        assert_eq!(snap[0].started_at_ms, 2000, "started_at 由首次观察决定，不随对账重置");

        reg.reconcile(false, false, 4000);
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn reconcile_does_not_reset_started_at() {
        let reg = TaskRegistry::new();
        reg.reconcile(true, false, 111);
        reg.reconcile(true, false, 999);
        assert_eq!(reg.snapshot()[0].started_at_ms, 111);
    }

    #[test]
    fn reconcile_refuses_new_entries_while_exiting() {
        let reg = TaskRegistry::new();
        reg.reconcile(true, false, 1);
        assert!(reg.try_begin_exiting());
        // Exiting 期间新冒出来的任务不登记……
        reg.reconcile(true, true, 2);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1, "Exiting 阶段不新增条目");
        assert_eq!(snap[0].id, "shell-update");
        // ……但已经消失的照常删除
        reg.reconcile(false, true, 3);
        assert!(reg.snapshot().is_empty());
    }
}
