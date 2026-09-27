# DSH Desktop —— 长任务退出拦截与安全清理机制（设计文档）

> 版本基线：DSH-Desktop 1.3.8（Tauri 2.11.5，Rust 2021，MSRV 1.77.2）
> 本文所有锚点均指向当前真实代码：`src-tauri/src/{lib,process,config,safe}.rs`、`src/{main.js,index.html}`。
>
> **修订 v2（2026-09-27，评审后定稿）**：①拍板：DSH 服务进程本身**不参与拦截**（§1.3，删除 `TaskKind::Service`）；
> ②DSH 切片改 fail-safe 三分支，任务 id/开始时间以 DSH 侧为权威（§4）；③弹窗握手改「ack/决定」两段式 + 再点逃生门（§5.4/§6.3）；
> ④常规退出优雅预算 3s→10s（对齐上游），`perform_exit` 移入 helper 线程防主线程冻结（§5.2/§5.5/§7.1）；
> ⑤§6.4/§1.1 关机链路按**源码级核验**改写（tao #1157 版本矩阵、`LoopDestroyed` 同步派发、`ExitRequested` 不触发）；
> ⑥§11 契约细化（任务 id / launch token 鉴权 / 判据复用上游）；⑦新增关机路径 `RunEvent::Exit` 兜底 sidecar 落盘（§5.3/§9.1）。
> 本文供未参与评审的实施 agent 直接执行，所有决策已内联，无需回看评审记录。

---

## 0. 需求 → 方案映射

| 需求 | 落点 |
|---|---|
| 退出拦截闸（X+quit / 托盘退出 / 系统关机） | `exit_guard::request_exit`（§5），接入点 = `lib.rs` 现有三处退出路径 |
| `close_action == tray` 不过闸 | `lib.rs` CloseRequested 分支保持不变（§5.3） |
| 任务状态检查 / 拦截弹窗 / 实时刷新 | `tasks::TaskRegistry`（§4）+ 前端 `exit-confirm-modal`（§10） |
| 无活跃任务静默放行 | `request_exit` 里 snapshot 为空 → 直接 `perform_exit`（§5.2） |
| 【继续后台运行】= 取消退出 + 藏托盘 | 交互线程 KeepRunning 分支（§5.4） |
| 优雅终止 → 超时强杀进程树 → 落盘 → app.exit | `process::shutdown_dsh_tree`（§7.1）+ `perform_exit`（§5.5；**落盘在杀进程之前**，顺序对需求原文的修正理由见 §9.1） |
| 防死锁（prevent_default / emit+invoke / 3s 降级 rfd） | §6 时序图 + 防死锁清单（第 10/11 条：ack≠决定两段式握手、再点逃生门）；`api.prevent_close()`；降级用**已有的** tauri-plugin-dialog，不引入 rfd |
| 系统关机不弹前端 | §6.4：关机路径零 UI，走 `RunEvent::Exit` 短超时清理 + Job Object 内核兜底 |
| Windows Job Object / Unix 进程组 | §7.2（Job 已存在，补 `TerminateJobObject` 显式强杀）；§7.4（`CommandExt::process_group` + `killpg`） |
| 全局退出锁 | `TaskRegistry.phase` 三态 CAS（§8，选型论证） |
| `last_session_killed_tasks` + 启动通知 | `exit-state.json` sidecar（perform_exit 主写 + `RunEvent::Exit` 关机兜底写，§9.1）+ `get_last_session_killed` 命令（§9） |

---

## 1. 现状盘点（不改不行的地方 / 已经能复用的地方）

### 1.1 现有真实退出路径（全部在 `lib.rs`）

| 路径 | 现状 | 问题 |
|---|---|---|
| 点 X 且 `close_action == "quit"`（`lib.rs:249-256`） | 直接 `window.app_handle().exit(0)` | 无任何任务检查 |
| 托盘菜单「退出」（`lib.rs:221-223`） | 直接 `app_handle.exit(0)` | 同上 |
| 系统关机/注销 | tao 收 `WM_ENDSESSION` → **同步派发** `LoopDestroyed` → `RunEvent::Exit`（尽力执行；`ExitRequested` **不触发**）。当前锁 tao 0.35.3 = 旧行为（派发后事件循环自然退出）；#1157（派发后再 `std::process::exit(0)`）首发于 tao 0.37.0 / tauri 2.12.0，详见 §6.4 | `cleanup_sync` 在此路径有机会执行（~5 s 预算）；`persist_before_exit`（挂在 ExitRequested）被跳过。sidecar 由 `RunEvent::Exit` 兜底写补齐（§9.1）；唯一硬保证 = Job Object 内核语义（§7.2） |
| 点 X 且 `close_action == "tray"`（`lib.rs:257-261`） | `api.prevent_close()` + `window.hide()` | **正确，保持不变**（需求二分支） |

> **术语对齐**（需求方案原文用词不同）：方案的 `closeBehavior: hide/quit` 对应本仓库 `close_action: "tray"/"quit"`（`config.rs:50,113`）；
> 方案的「`event.prevent_default()`」对应 Rust 侧 `api.prevent_close()`；方案的 rfd 降级对话框由仓库已有的 `tauri-plugin-dialog` 承担（`Cargo.toml:25`），不引入 rfd。

### 1.2 现有可复用件（本设计不重复造轮子）

| 已有件 | 位置 | 本设计中的角色 |
|---|---|---|
| Windows Job Object（`KILL_ON_JOB_CLOSE`） | `process.rs:22-102` `win` 模块；`start_internal:1576-1581` 已在 spawn 后把 DSH 挂上 | 进程树强杀的**主**手段（§7.2），崩溃/断电场景内核兜底 |
| `taskkill /PID <pid> /T /F` | `process.rs:500 run_taskkill` | Job 句柄缺失时的降级强杀 |
| `stop_internal`（taskkill → 3s 回收 → 关 Job → 销毁内嵌页） | `process.rs:1802` | 被升级为 `shutdown_dsh_tree`（加优雅阶段与强杀语义），原函数保留为停止按钮路径 |
| `cleanup_sync`（幂等兜底，含安全模式实例） | `process.rs:2865` | `RunEvent::Exit` 的最终兜底，不动 |
| 原子写盘 `write_atomic` | `config.rs:1085` | `exit-state.json` 落盘复用（半截文件 = 报告丢失，必须原子） |
| 弹窗 ↔ 内嵌 webview 显隐协调 | `main.js` `MODALS`/`showModal`/`syncWebviewVisibility`，`process.rs set_dsh_webview_visible`（先记意图再行动） | 退出弹窗加入 `MODALS` 即可，不再重蹈安全模式灰暗的覆辙 |
| `tauri-plugin-dialog` | 已在 `Cargo.toml` | WebView 无响应时的原生降级对话框（`blocking_show`），**不需要引入 rfd** |
| 双语文案 `i18n.rs` | 全量 key 表 | 所有新增用户可见文案（§10.3） |
| 壳侧长任务标志 | `AppState.updating`（更新 DSH）、`AppState.setup_busy`（引导安装 Node/DSH/pnpm/Python） | 纳入 TaskRegistry 的第一批**真实存在**的长任务 |

### 1.3 关键事实：Agent 任务跑在 DSH 进程里，壳并不天然知道

「生成周报 PPT / 终端命令 / 文件写入」都是 DSH（Node 服务）内部状态，桌面壳的 Rust 端看不到。
因此 TaskRegistry 设计为**多数据源注册表**：

- **壳内源（今天就有）**：`updating`（更新 DSH）、`setup_busy`（引导安装）、（将来）更多壳任务；
- **DSH 内源（需约定契约）**：DSH 暴露 `GET /api/desktop/tasks`，壳以 2 s 轮询替换注册表中的 `dsh:*` 切片（§11）。
  服务端判据可直接复用上游官方 Electron 壳的进程内检查逻辑
  （`apps/desktop-host/src/quit-inspection.ts` 的 `hasDesktopActiveTasks`：任一 agent `status==='running'`
  或 inbox 有排队轮次、任一 job running/stopping、schedule 任务）。

**拍板（评审决策，选项 a）：DSH 服务进程本身不是「任务」，不注册进注册表。**
早期草稿中的 `TaskKind::Service` 已删除：「DSH 在跑」≠「有任务要保护」，若把服务进程算作可拦截任务，
则只要 DSH 存活，**每次退出都弹窗**，直接违背需求一.3「无活跃任务静默放行」，功能退化成骚扰。
官方 Electron 壳（apps/desktop）「每次真退出必弹原生确认框」是另一种产品哲学，本项目**不采纳**。
代价如实承认：§11 契约落地之前，本机制对 DSH 内 Agent 任务的可见性为零，
此时的退出行为与今天完全一致（不算倒退，但也没有新保护）；壳内源覆盖的
更新 DSH / 引导安装两类长任务是今天真实存在的，属纯增量收益。
**因此实施顺序要求：DSH 契约先行（或与壳侧并行推进），壳侧闸随后生效。**

- **降级约定（fail-safe，对齐上游「unknown 按有任务处理」的哲学）**：
  - DSH 不在运行（status != "running"，含 running-external）→ 权威地**清空** `dsh:*` 切片；
  - 404（契约未实现）→ 清空切片并**停止探测**（只记一次日志），退化为仅壳内任务参与拦截；
  - 401/403（令牌失效等系统性故障）→ 同 404 处理（清空 + 停止探测 + 记错误日志）；
  - DSH 活着但请求**超时/5xx**（状态问不到）→ **保留上一轮切片**（视为 stale）：
    宁可多拦一次，也不让已知在跑的任务从清单里静默消失被腰斩；
  - **绝不因数据源缺失而凭空造一个「DSH 忙」的假任务**，那会让所有用户的每次退出都弹窗。

---

## 2. 总体架构

```
┌────────────────────────── 壳进程（Rust） ──────────────────────────┐
│                                                                    │
│  lib.rs 三处退出触发点                                              │
│   ① CloseRequested(quit)  ② 托盘 quit_app  ③ 系统关机(tao→直接exit)    │
│          │                      │                    │             │
│          ▼                      ▼                    │             │
│   exit_guard::request_exit(source)                    │             │
│          │                                            │             │
│   TaskRegistry（tasks.rs）                            │             │
│    phase: Idle ──CAS──▶ Intercepting ──CAS──▶ Exiting ◀──try_cas────┤
│    tasks: BTreeMap<id, TaskSnapshot>（壳内源 + dsh:* 轮询切片）      │
│          │                                            │             │
│   无任务 ──────────────▶ perform_exit(helper 线程) ◀── 强制退出 ───┘             │
│   有任务 ── emit exit-confirm-show ──▶ 前端弹窗（3s ack 超时）       │
│                              │ ack 超时/WebView 死                      │
│                              ▼                                     │
│                    tauri-plugin-dialog 原生框（helper 线程）        │
│                                                                    │
│   perform_exit: 藏窗 → 案发快照落盘 → shutdown_dsh_tree(优雅10s→强杀)      │
│                 → app.exit(0)（布局落盘由 ExitRequested 在主线程做）                │
│   RunEvent::ExitRequested/Exit: 既有 cleanup_sync 幂等兜底（不动）  │
└────────────────────────────────────────────────────────────────────┘
```

退出状态机（`TaskRegistry.phase`，AtomicU8）：

```
Idle(0) ──try_begin_intercept(CAS 0→1)──▶ Intercepting(1)
   ▲                                        │
   │            cancel_intercept(1→0)       │ try_begin_exiting(CAS 1→2 或 0→2)
   └────────────────────────────────────────┴──────────────▶ Exiting(2)【终态，进程随之退出】
```

- `Intercepting`：弹窗期间。新任务注册 → 壳内源**拒绝**（`Registration::Blocked`）；DSH 切片新任务**标记** `abort_on_exit: true`（弹窗里实时可见「即将中断」）。
- `Exiting`：清理中。一切注册直接拒绝；`try_begin_exiting` 只允许成功一次 → `perform_exit` 天然防重入。
- 系统关机路径**绕过** Intercepting：直接 `try_begin_exiting()`（从 0 或 1 强推到 2，见 §6.4 的竞态处理）。

---

## 3. 新增文件与改动面

```
src-tauri/src/tasks.rs        [新增] TaskRegistry + DSH 任务轮询器
src-tauri/src/exit_guard.rs   [新增] 退出流程（request_exit / 交互握手 / perform_exit / sidecar）
src-tauri/src/process.rs      [改]   win::terminate_job、unix 模块、shutdown_dsh_tree、
                                     update/setup 注册壳任务（start_internal **不**注册任务，见 §1.3 拍板）
src-tauri/src/lib.rs          [改]   manage 两个新状态、3 个新命令、CloseRequested/托盘分支接线、
                                     spawn 轮询器；RunEvent::Exit 兜底加一笔
                                     persist_exit_state_fallback（§9.1），其余兜底不动
src-tauri/src/config.rs       [改]   （仅复用 write_atomic，无新配置键）
src-tauri/Cargo.toml          [改]   [target.'cfg(unix)'.dependencies] libc = "0.2"（tauri 在 unix 目标
                                     本就传递依赖它，锁文件里已有，增量≈0）
src/index.html                [改]   exit-confirm-modal 标记
src/main.js                   [改]   MODALS + 监听/轮询/按钮 + 启动横幅
src/i18n.rs + src/i18n.js     [改]   新 key（§10.3）
```

---

## 4. TaskRegistry（`tasks.rs` 骨架）

```rust
//! 任务注册表 + 全局退出锁。
//!
//! 并发选型（论证见设计文档 §8）：std::sync::Mutex + AtomicU8/AtomicBool，
//! 不引入 tokio —— 本 crate 全部后台流是 std::thread（start_internal /
//! spawn_log_reader / wait_ready_and_embed），临界区内没有 await 点。

use serde::Serialize;
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_INTERCEPTING: u8 = 1;
pub const PHASE_EXITING: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Agent,       // DSH 内：Agent 任务（生成文档等）
    Terminal,    // DSH 内：终端命令
    FileWrite,   // DSH 内：文件写入中
    Update,      // 壳内：更新 DSH（npm，可能数分钟）
    Install,     // 壳内：引导安装（node/dsh/pnpm/python）
    // 注意：没有 Service 变体 —— DSH 服务进程本身不参与拦截（§1.3 拍板）
}

#[derive(Clone, Serialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub kind: TaskKind,
    /// 已本地化的任务标题（如「生成周报 PPT」「更新 DSH」）
    pub title: String,
    /// 开始时间（epoch ms）：壳内任务 = 注册时刻；`dsh:*` 切片 = §11 契约回包的
    /// DSH 侧权威值（壳**不**重置）。前端据此本地计时，无需每次轮询重取
    pub started_at_ms: u64,
    /// Intercepting 期间新出现的任务：退出将中断它（前端标灰/加标签）
    pub abort_on_exit: bool,
}

/// DSH 切片中的一条任务：§11 契约回包的镜像。**id 与开始时间以 DSH 侧为权威**，
/// 壳不重发号、不重置时钟 —— 这是弹窗列表稳定（不跳动）与时长真实的前提
/// （旧稿每轮轮询用 next_seq 重发号 + now_ms() 重置时钟，时长会永远显示 ≤2 s，评审已修正）。
#[derive(Clone)]
pub struct DshTaskEntry {
    pub id: String,           // DSH 侧任务 id（session / turn / job id）
    pub kind: TaskKind,
    pub title: String,
    pub started_at_ms: u64,   // epoch ms，DSH 侧权威值
}

pub enum Registration {
    /// 已注册，返回 id
    Accepted(String),
    /// Exiting：一律拒绝
    Blocked,
    /// Intercepting：注册成功但标记为「退出时中断」（仅 DSH 切片使用；
    /// 壳内长任务在弹窗期间直接 Blocked —— 它们都由将被遮住的设置页发起）
    MarkedAbortOnExit(String),
}

pub struct TaskRegistry {
    next_seq: AtomicU64,
    phase: AtomicU8,
    /// BTreeMap：快照按 id 稳定排序，前端列表不会跳动
    tasks: Mutex<BTreeMap<String, TaskSnapshot>>,
    /// DSH 内源本轮是否可用（404/超时 → false，用于日志与诊断，不参与拦截判定）
    dsh_source_alive: AtomicBool,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(1),
            phase: AtomicU8::new(PHASE_IDLE),
            tasks: Mutex::new(BTreeMap::new()),
            dsh_source_alive: AtomicBool::new(false),
        }
    }

    pub fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }

    /// —— 阶段闸（全局退出锁的核心，全部无锁 CAS，绝不碰 tasks 锁） ——
    pub fn try_begin_intercept(&self) -> bool {
        self.phase
            .compare_exchange(PHASE_IDLE, PHASE_INTERCEPTING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
    pub fn cancel_intercept(&self) {
        let _ = self.phase.compare_exchange(
            PHASE_INTERCEPTING, PHASE_IDLE, Ordering::SeqCst, Ordering::SeqCst,
        );
    }
    /// Idle/Intercepting → Exiting；返回 true 的一方负责执行 perform_exit。
    /// 系统关机与用户强退并发时，由此天然裁决唯一执行者。
    pub fn try_begin_exiting(&self) -> bool {
        self.phase
            .compare_exchange(PHASE_INTERCEPTING, PHASE_EXITING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            || self
                .phase
                .compare_exchange(PHASE_IDLE, PHASE_EXITING, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }

    /// —— 任务注册 / 结束 ——
    pub fn register(&self, kind: TaskKind, title: &str) -> Registration {
        if self.phase() == PHASE_EXITING {
            return Registration::Blocked;
        }
        let id = format!("shell-{}", self.next_seq.fetch_add(1, Ordering::Relaxed));
        let snap = TaskSnapshot {
            id: id.clone(),
            kind,
            title: title.to_string(),
            started_at_ms: now_ms(),
            abort_on_exit: false,
        };
        self.tasks.lock().unwrap().insert(id.clone(), snap);
        Registration::Accepted(id)
    }
    pub fn finish(&self, id: &str) {
        self.tasks.lock().unwrap().remove(id);
    }

    /// DSH 切片整表替换（轮询器**只在拿到权威回包**时调用，见 §4.1 的 DshFetch 分支）。
    /// 键 = `dsh:<DSH侧任务id>`：同一任务跨轮询保持稳定 → 前端列表不跳动、时长真实累计；
    /// abort_on_exit 只标 Intercepting 期间**新出现**的 id（弹窗展示时已在列的任务不标）。
    pub fn replace_dsh_slice(&self, entries: Vec<DshTaskEntry>) {
        let intercepting = self.phase() == PHASE_INTERCEPTING;
        let mut map = self.tasks.lock().unwrap();
        let prev: std::collections::BTreeSet<String> =
            map.keys().filter(|k| k.starts_with("dsh:")).cloned().collect();
        map.retain(|id, _| !id.starts_with("dsh:"));
        for e in entries {
            let id = format!("dsh:{}", e.id);
            let abort_on_exit = intercepting && !prev.contains(&id);
            map.insert(id.clone(), TaskSnapshot {
                id, kind: e.kind, title: e.title,
                started_at_ms: e.started_at_ms,   // DSH 侧权威值，透传
                abort_on_exit,
            });
        }
    }

    /// 权威地清空切片（DSH 不在运行 / 404 契约缺失 / 401 鉴权失效）。
    /// 请求超时或 5xx 时**不要**调用 —— 保留旧切片（stale）才是 fail-safe 语义（§1.3）。
    pub fn clear_dsh_slice(&self) {
        self.tasks.lock().unwrap().retain(|id, _| !id.starts_with("dsh:"));
    }

    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        self.tasks.lock().unwrap().values().cloned().collect()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- DSH 内源轮询器（§4.1，fail-safe 三分支见 §1.3 降级约定） ----------

/// 一次拉取的结果分类：把「权威空列表」与各种失败语义分开，失败分支的处置**不同**
pub enum DshFetch {
    /// 200 + 合法 JSON：权威快照（可能为空列表）
    Ok(Vec<DshTaskEntry>),
    /// 404（契约未实现）/ 401 / 403（令牌失效等系统性故障）→ 清空切片并**停止探测**
    ContractUnavailable,
    /// 连接失败 / 超时 / 5xx，但 DSH 进程活着：状态问不到 → **保留旧切片**（stale），
    /// 宁可多拦一次，不让已知在跑的任务静默消失（对齐上游「unknown 按有任务处理」）
    Transient,
}

/// setup 时启动：每 2 s 问一次 DSH「现在有什么任务」。
/// 只在 DSH status == "running" 时轮询（**不含 running-external**：外部服务
/// 不是我们的进程树，也不在我们的 launch token 覆盖范围内）。
pub fn spawn_dsh_task_poller(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        use tauri::Manager;
        let mut probing_stopped = false;   // 404/401 闩锁：不再无谓地每 2 s 打一次
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if probing_stopped { continue; }
            let registry = app.state::<TaskRegistry>();
            if crate::process::current_status(&app) != "running" {
                registry.clear_dsh_slice();          // 权威：DSH 不存在 → 无任务
                continue;
            }
            let (port, token) = dsh_endpoint_of(&app);
            match fetch_dsh_tasks(port, token.as_deref()) {
                DshFetch::Ok(entries) => registry.replace_dsh_slice(entries),
                DshFetch::ContractUnavailable => {
                    probing_stopped = true;
                    registry.clear_dsh_slice();
                    crate::process::log_launcher(&app,
                        "[tasks] /api/desktop/tasks 不可用(404/401)：DSH 任务契约未落地，退化为仅壳内任务拦截".to_string());
                }
                DshFetch::Transient => { /* fail-safe：保留旧切片，什么都不做 */ }
            }
        }
    });
}

/// 轮询/优雅关闭共用的端点解析：优先 `AppState.detected_url`（pub 字段，
/// 形如 `Some((url_with_token, port))` —— DSH 输出行解析而来，是实际监听地址的
/// 真值，含 launch token），缺失时退回 `config::load(app).port` + 无令牌。
/// pub(crate)：§7.1 graceful_signal 的 http_post_shutdown 同样用它。
pub(crate) fn dsh_endpoint_of(app: &tauri::AppHandle) -> (u16, Option<String>) {
    // 实现要点：从 detected_url 的 URL query 里取 `token=` 参数；解析失败按无令牌处理
    unimplemented!("见注释")
}

/// GET http://127.0.0.1:<port>/api/desktop/tasks?token=… → DshFetch
/// 手写 TcpStream GET（与 process::http_ready 同款风格，零新依赖），800 ms 超时。
/// 响应 schema 见 §11：{"tasks":[{"id":"…","kind":"agent|terminal|file_write","title":"…","started_at_ms":…}]}
fn fetch_dsh_tasks(port: u16, token: Option<&str>) -> DshFetch {
    let _ = (port, token);
    unimplemented!("见注释")
}
```

---

## 5. 退出拦截流程（`exit_guard.rs` 骨架）

### 5.1 状态与入口

```rust
use crate::process;
use crate::tasks::{self, TaskRegistry, TaskSnapshot, PHASE_INTERCEPTING};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// 前端 **ack** 窗口：3 秒内收不到「渲染完成回执」= WebView 崩溃/冻结/监听未注册，
/// 降级原生对话框。注意：这个超时检测的是「前端活没活」，**不是**「用户决定没决定」——
/// ack 之后无限期等待用户决定（§5.4 两段式握手；旧稿单段 3 s 超时是设计错误，
/// 会把犹豫超过 3 秒的正常用户头上再弹一个原生框）。
pub const FRONTEND_ACK_TIMEOUT: Duration = Duration::from_secs(3);
/// 优雅终止等待（常规退出）：对齐上游 apps/desktop `host-process.ts` 的 10 s
/// （DSH 自拆 + 会话落盘需要时间，3 s 容易掐断写盘）。此时弹窗已确认过用户意图、
/// 主窗口已隐藏（§5.5），多等几秒无感知成本。系统关机用更短预算。
pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(10);
pub const SHUTDOWN_GRACEFUL_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExitSource { Close, Tray, System }
impl ExitSource {
    pub fn as_str(self) -> &'static str {
        match self { Self::Close => "close", Self::Tray => "tray", Self::System => "system" }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExitKind { Normal, Forced, System }

#[derive(Clone, Serialize)]
pub struct ExitConfirmPayload {
    pub tasks: Vec<TaskSnapshot>,
    pub source: String,
}

enum Decision { Ack, ForceExit, KeepRunning }

/// pending 应答通道：先存 sender 再 emit（防「前端先答、sender 未就位」的竞态）。
/// `reclick`：弹窗期间用户再次点 X/托盘时置 true —— 这是「前端在 ack 之后死掉」
/// 场景的逃生门入口（消费逻辑见 §5.4 心跳分支）。
pub struct ExitGuardState {
    pending: StdMutex<Option<mpsc::Sender<Decision>>>,
    pub reclick: AtomicBool,
}
impl ExitGuardState {
    pub fn new() -> Self {
        Self { pending: StdMutex::new(None), reclick: AtomicBool::new(false) }
    }
}
```

### 5.2 统一入口 `request_exit`（三个触发点都汇到这里）

```rust
pub fn request_exit(app: &AppHandle, source: ExitSource) {
    let registry = app.state::<TaskRegistry>();

    // 系统关机：绝不走交互（OS 给 GUI 的预算只有几秒，弹前端 = 被判定无响应遭强杀）。
    // 同时它必须能从 Intercepting 里**强推**进 Exiting（用户正开着弹窗时系统关机）。
    if source == ExitSource::System {
        if registry.try_begin_exiting() {
            // 唤醒可能还在等应答的交互线程：塞入 ForceExit，让它直接返回，不再弹原生框
            if let Some(tx) = app.state::<ExitGuardState>().pending.lock().unwrap().take() {
                let _ = tx.send(Decision::ForceExit);
            }
            perform_exit(app, ExitKind::System);
        }
        return;
    }

    // 交互路径：Idle → Intercepting 只放一个进来（X 与托盘并发点击天然互斥）
    if !registry.try_begin_intercept() {
        // 已在弹窗 / 已在退出：不开第二个流程，但记下「再点了一次」。
        // 若前端还活着，这只是用户手抖（§5.4 心跳会先重发事件验证，不会误弹原生框）；
        // 若前端在 ack 后死掉，这就是用户唯一的逃生门。
        app.state::<ExitGuardState>().reclick.store(true, Ordering::SeqCst);
        return;
    }

    let snapshot = registry.snapshot();
    if snapshot.is_empty() {
        // 需求：无活跃任务 → 静默放行。清理（含最长 10 s 优雅等待）放进 helper 线程：
        // 主线程立即回到事件循环，窗口不会在退出期间冻结成「(未响应)」幽灵
        //（perform_exit 的线程契约见 §5.5）。
        if registry.try_begin_exiting() {
            let app2 = app.clone();
            std::thread::spawn(move || perform_exit(&app2, ExitKind::Normal));
        }
        return;
    }

    // 有活跃任务 → 交互必须发生在独立线程。
    // X 路径的当前线程是主线程：在这里 recv_timeout / blocking_show = 主线程被占住
    // → WebView 事件循环冻结 → 弹窗永远渲染不出来 → 死锁。
    spawn_interaction(app.clone(), source);
}
```

### 5.3 `lib.rs` 接线（骨架 diff）

```rust
// .manage(...) 增加两项（在 AppState/SafeState 旁）：
.manage(tasks::TaskRegistry::new())
.manage(exit_guard::ExitGuardState::new())

// invoke_handler 增加：
exit_guard::get_active_tasks,
exit_guard::exit_decision,
exit_guard::get_last_session_killed,

// setup 尾部启动 DSH 任务轮询器：
tasks::spawn_dsh_task_poller(app.handle().clone());

// ---- ① 点 X（lib.rs:249 现有分支改写） ----
WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
    let quit_on_close = {
        let app = window.app_handle();
        config::load(app).close_action.trim().eq_ignore_ascii_case("quit")
    };
    if quit_on_close {
        // 先拦下，再走闸：无任务时 perform_exit 里的 app.exit(0) 会销毁窗口，
        // 与旧行为等价；有任务时窗口必须活着渲染弹窗。
        api.prevent_close();
        exit_guard::request_exit(window.app_handle(), exit_guard::ExitSource::Close);
    } else {
        // close_action == "tray"：维持现状，**不过闸**
        api.prevent_close();
        let _ = window.hide();
    }
}

// ---- ② 托盘退出（lib.rs:221 现有分支改写） ----
"quit_app" => {
    exit_guard::request_exit(app_handle, exit_guard::ExitSource::Tray);
}

// ---- ③ 系统关机：零 UI、零交互。tao 收 WM_ENDSESSION → 同步派发 LoopDestroyed
//         → RunEvent::Exit（尽力执行；ExitRequested 不触发，§6.4）。
//         既有 Exit 兜底只加一笔：exit_guard::persist_exit_state_fallback ——
//         此刻 exit-state.json 若不存在（= 没经过 perform_exit），补写一条
//         exit_kind:"system" 的兜底记录（§9.1），随后 cleanup_sync 照旧。
//         ~5 s 预算容纳 taskkill + 短回收，但不是保证 —— 孤儿防护的唯一硬保证
//         = Job Object KILL_ON_JOB_CLOSE（壳进程亡 → 内核关句柄 → 整树歼灭，§7.2）。
//         request_exit(System) 仅供将来挂接 WM_QUERYENDSESSION 增强（§6.4），本期不接线。
```

### 5.4 交互线程（emit → ack → 无限期等决定 → ack 超时降级）

```rust
fn spawn_interaction(app: AppHandle, source: ExitSource) {
    std::thread::spawn(move || {
        let registry = app.state::<TaskRegistry>();

        // 托盘路径通常发生在窗口隐藏时：先唤回主窗口再弹（窗口操作必须主线程）
        if source == ExitSource::Tray {
            crate::lib_show_main_window(&app); // 将 lib.rs 的 show_main_window 改 pub(crate)
        }

        // 清掉上一轮遗留的逃生门标志（取消退出 → 再触发之间的误点会被带进来）
        app.state::<ExitGuardState>().reclick.store(false, Ordering::SeqCst);

        let payload = ExitConfirmPayload {
            tasks: registry.snapshot(),
            source: source.as_str().to_string(),
        };
        let (tx, rx) = mpsc::channel::<Decision>();
        *app.state::<ExitGuardState>().pending.lock().unwrap() = Some(tx);
        // 先就位 sender，再 emit —— 顺序不能反
        let _ = app.emit("exit-confirm-show", payload.clone());

        // 两段式握手：ack ≠ 决定。
        // 第一段：3 s 内等前端「渲染完成」回执（前端在弹窗显示后立即 invoke exit_decision{"ack"}）；
        // 第二段：无限期等用户决定 —— 用户在弹窗前犹豫多久都不允许再弹第二个原生框。
        // 5 s 心跳检查 reclick（逃生门）：再点 X/托盘时先给前端一次「重发」机会
        //（re-emit → 活着就会再 ack 一次），3 s 仍无回音 → 判前端已死，降级原生对话框。
        let decision = match rx.recv_timeout(FRONTEND_ACK_TIMEOUT) {
            Ok(Decision::Ack) => loop {
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(d) => break d,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if app.state::<ExitGuardState>().reclick.swap(false, Ordering::SeqCst) {
                            let _ = app.emit("exit-confirm-show", payload.clone());
                            match rx.recv_timeout(FRONTEND_ACK_TIMEOUT) {
                                Ok(Decision::Ack) => {}   // 前端活着：继续无限期等决定
                                Ok(d) => break d,         // 直接收到了决定
                                Err(_) => break native_fallback_dialog(&app, &registry),
                            }
                        }
                    }
                    // 通道断开（理论上不会：sender 存在 managed state 里）：保守按「不退出」
                    Err(mpsc::RecvTimeoutError::Disconnected) => break Decision::KeepRunning,
                }
            },
            Ok(d) => d,   // force/keep 先于 ack 到达（前端没发 ack）→ 直接采纳
            Err(_) => {
                // 3 s 连 ack 都没收到 = WebView 崩溃 / 页面被冻结 / 监听未注册。
                // tauri-plugin-dialog 的 blocking_show 文档明确要求不在主线程调用 ——
                // 我们就在这个 helper 线程上弹，不碰主线程。
                native_fallback_dialog(&app, &registry)
            }
        };
        *app.state::<ExitGuardState>().pending.lock().unwrap() = None;
        let _ = app.emit("exit-confirm-hide", ());

        match decision {
            Decision::KeepRunning => {
                // 仅当仍是 Intercepting 才回退（系统关机可能已强推进 Exiting）
                registry.cancel_intercept();
                let _ = app.emit("exit-registry-unlocked", ());
                if source == ExitSource::Close {
                    // 需求：取消本次退出 → 隐藏窗口至托盘，任务继续
                    let app2 = app.clone();
                    let _ = app.run_on_main_thread(move || {
                        if let Some(w) = process::main_window_handle(&app2) {
                            let _ = w.hide();
                        }
                    });
                }
                // Tray 来源：保持窗口可见（用户本来就是从托盘发起的）
            }
            Decision::ForceExit => {
                if registry.try_begin_exiting() {
                    perform_exit(&app, ExitKind::Forced);
                }
                // try_begin_exiting 失败 = 系统关机已抢先（它已在 perform_exit），静默返回
            }
        }
    });
}

fn native_fallback_dialog(app: &AppHandle, registry: &TaskRegistry) -> Decision {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let tasks = registry.snapshot();
    let body = if tasks.is_empty() {
        i18n::t("exit_fallback_none").to_string()
    } else {
        let mut s = i18n::t("exit_fallback_body").to_string();
        for t in tasks.iter().take(5) {
            s.push_str(&format!("\n· {}", t.title));
        }
        s
    };
    // 阻塞式原生 MessageBox：标题/正文/按钮全部走 i18n（zh/en）
    let force = app.dialog()
        .message(body)
        .title(i18n::t("exit_fallback_title"))
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            i18n::t("exit_btn_force").to_string(),   // 确定 = 强制退出
            i18n::t("exit_btn_keep").to_string(),    // 取消 = 继续后台运行
        ))
        .blocking_show();
    if force { Decision::ForceExit } else { Decision::KeepRunning }
}
```

### 5.5 `perform_exit`（清理与退出的唯一入口）

```rust
/// **线程契约**：perform_exit 恒在**非主线程**执行 —— 静默放行路径由 §5.2 spawn，
/// 强退路径本就在交互线程。唯一例外是 System（主线程内联，1.5 s 短预算，
/// 必须赶在 OS 强杀前完成，来不及也没有必要切线程）。
/// 因此函数内一切窗口/webview 操作必须经 `run_on_main_thread` 投递（此时主线程空闲，
/// 会 promptly 执行）；**绝不可直接调**需要主线程的函数 —— 前车之鉴：
/// `window_state::persist_before_exit` → `restore_daily_layout` 直接操作窗口，
/// 其注释明写「已在主线程（RunEvent 回调）」。
pub fn perform_exit(app: &AppHandle, kind: ExitKind) {
    let registry = app.state::<TaskRegistry>();
    process::log_launcher(app, &i18n::fmt("log_exit_kind", &[&kind_label(kind)]));

    // 0) 先隐藏主窗口（best-effort）：下面的优雅等待最长 10 s，用户的感知应该是
    //    「点了退出，窗口立刻没了」，而不是盯着一个不再重绘的窗口。
    //    hide() 不改变几何，不影响之后 ExitRequested 里的布局落盘。
    //    System 路径跳过：OS 正在销毁会话，隐藏没有意义。
    if kind != ExitKind::System {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = process::main_window_handle(&app2) { let _ = w.hide(); }
        });
    }

    // 1) 案发现场先落盘（§9）：必须发生在杀进程**之前**——
    //    之后哪怕断电/被 OS 强杀，磁盘上也已有这份记录。写失败不阻断退出。
    persist_exit_state(app, kind, registry.snapshot());

    // 2) 优雅终止 → 超时强杀整棵进程树（§7）
    let graceful = if kind == ExitKind::System { SHUTDOWN_GRACEFUL_TIMEOUT } else { GRACEFUL_TIMEOUT };
    process::shutdown_dsh_tree(app, graceful);

    // 3) 退出。app.exit(0) 任意线程可调，会在主线程触发 RunEvent::ExitRequested，
    //    既有的 persist_before_exit + cleanup_sync 幂等兜底在那里执行（lib.rs:306-309）。
    //    布局落盘**不在这里重复做**：旧稿第 3 步直接调 persist_before_exit，
    //    在 helper 线程上违反其主线程契约（评审已修正——交给 ExitRequested 在主线程做）。
    app.exit(0);
}

/// 命令：前端弹窗每秒轮询的活跃任务列表
#[tauri::command]
pub fn get_active_tasks(app: AppHandle) -> Vec<TaskSnapshot> {
    app.state::<TaskRegistry>().snapshot()
}

/// 命令：前端按钮回传用户选择（非阻塞：只投递 channel，立即返回）
#[tauri::command]
pub fn exit_decision(app: AppHandle, decision: String) -> Result<(), String> {
    let d = match decision.as_str() {
        "ack" => Decision::Ack,          // 前端「渲染完成」回执（§5.4 第一段握手）
        "force" => Decision::ForceExit,
        "keep" => Decision::KeepRunning,
        _ => return Err("invalid decision".into()),
    };
    if let Some(tx) = app.state::<ExitGuardState>().pending.lock().unwrap().take() {
        let _ = tx.send(d);
    }
    Ok(()) // 重复应答/无人在等：静默成功（幂等）
}
```

---

## 6. Rust ↔ 前端事件流与防死锁（输出要求 2）

### 6.1 完整时序：点 X + quit + 有任务

```
用户            主线程(lib.rs)          交互线程                前端(main.js)         DSH 进程
 │ 点X  ─────────▶ CloseRequested
 │                 prevent_close()
 │                 try_begin_intercept(CAS 0→1) ✓
 │                 snapshot() 非空
 │                 spawn_interaction ──────▶ │
 │                 （主线程立即空闲！）       │ Tray?→show 窗口
 │                                           │ 存 sender → emit "exit-confirm-show"
 │                                           │      ─────────────────────▶ listen 收到
 │                                           │                              showModal('exit-confirm-modal')
 │                                           │                              （MODALS 机制隐藏 DSH 子 webview）
 │                                           │                              renderExitTasks()
 │                                           │                              setInterval 1s → invoke get_active_tasks
 │                                           │◀─ invoke exit_decision{ack} ─┤ 渲染完成回执（弹窗显示后立即发）
 │                                           │ recv_timeout(3s) → ack ✓     │
 │                                           │ 此后无限期等决定              │ 用户点按钮
 │                                           │（5s 心跳查 reclick 逃生门）   │ invoke exit_decision{force|keep}
 │                                           │◀───────────────────── invoke ┤ mpsc 投递，命令立即返回
 │                                           │ decision
 │                    ┌── keep ──────────────┤ cancel_intercept(1→0)
 │ 窗口藏托盘 ◀───────┤                      │ emit "exit-confirm-hide"
 │ 任务继续           └── force ─────────────▶ try_begin_exiting(1→2) ✓
 │                                           │ persist_exit_state(案发快照)
 │                                           │ shutdown_dsh_tree(10s 优雅→强杀) ────▶ HTTP /api/desktop/shutdown
 │                                           │ persist_before_exit                  → 超时 TerminateJobObject 整树
 │                                           │ app.exit(0)
 │                                           │        RunEvent::ExitRequested → persist+cleanup_sync（幂等）
 │                                           │        RunEvent::Exit → cleanup_sync（幂等）
```

### 6.2 无任务 / tray 关偏好 / 系统关机的时序差异

- **X + quit + 无任务**：主线程 prevent_close → CAS → snapshot 空 → try_begin_exiting → **spawn helper 线程**执行 `perform_exit(Normal)`（窗口先隐藏，主线程立即回事件循环）。与旧路径行为等价、无新增 UI 等待，且 10 s 清理不会把窗口冻成「(未响应)」。
- **X + `close_action=="tray"`**：根本不进 `request_exit`，`prevent_close()+hide()` 原样（不过闸）。
- **系统关机**：tao 收 `WM_ENDSESSION` → 同步派发 `LoopDestroyed` → `RunEvent::Exit`（`ExitRequested` 不触发；当前锁 tao 0.35.3 派发后事件循环自然退出，tauri 2.12+/tao 0.37+ 则派发后 `std::process::exit(0)`，§6.4）→ 兜底 sidecar 写（§9.1）+ `cleanup_sync` 在 ~5 s 预算内有机会执行（尽力而为）。零弹窗、零事件往返；孤儿防护的唯一硬保证 = Job Object `KILL_ON_JOB_CLOSE`（内核关句柄 → 整树歼灭，不依赖任何用户态回调）。`request_exit(System)` 是预留给 `WM_QUERYENDSESSION` 增强（§6.4）的同步入口，本期不接线。

### 6.3 防死锁清单（每条都对应一个真实死法）

1. **主线程只做三件事**：`prevent_close()`、CAS 置位、`std::thread::spawn`。主线程绝不 `recv_timeout` / `blocking_show` / `sleep` 等前端 —— 否则 WebView2 事件循环冻结，弹窗渲染不出来，ack 超时也不会醒来（交互线程还活着，但弹窗永远出不来，形成「可见的死锁」）。**10 s 清理同样不上主线程**：`perform_exit` 一律在 helper 线程执行（静默放行路径也 spawn），窗口隐藏经 `run_on_main_thread` 投递（§5.5 线程契约）。
2. **先存 sender 再 emit**：反过来会出现「前端极快应答、`pending` 还是 None」的丢应答竞态，退化成 ack 超时 + 多弹一个原生框。
3. **命令只碰 channel/原子量**：`exit_decision` 无锁等待、无 IO，前端 invoke 立即返回，不占 Tauri IPC 线程。
4. **降级对话框在 helper 线程弹**：`tauri_plugin_dialog::blocking_show` 官方要求不得在主线程调用；Windows 的模态消息泵在自己的线程上跑，与主事件循环互不阻塞。
5. **弹窗期间注册表处于 Intercepting**：用户犹豫时新任务被拒/标记 `abort_on_exit`，杜绝「快照之后、强杀之前」冒出新任务被腰斩的竞态（需求 4）。
6. **X/托盘并发**：`try_begin_intercept` CAS 只放一个流程进来；第二次点击被忽略（弹窗还开着，无感知）。
7. **交互与系统关机并发**：System 路径 `try_begin_exiting` 强推 1→2，同时向 pending 通道塞 `ForceExit` 唤醒交互线程 —— 交互线程自己的 `try_begin_exiting` 失败 → 静默返回，绝不会在关机时再弹原生框。
8. **锁纪律**：绝不持有任何 Mutex 跨 `emit`/`invoke`/`sleep`（本设计所有等待都发生在无锁状态）；TaskRegistry 持锁期间不回调 AppState（锁序单向，见 §8）。
9. **弹窗加入 `MODALS` 数组**：复用既有「弹窗打开 → 隐藏内嵌 DSH webview」机制（`syncWebviewVisibility` + Rust 侧先记意图再行动）。不加入的话，退出弹窗会被盖在原生子 webview 下面 —— 安全模式灰暗那次教训的镜像场景。
10. **两段式握手（ack ≠ 决定）**：`FRONTEND_ACK_TIMEOUT`（3 s）只检测「前端活没活」；收到 ack 后**无限期**等用户决定 —— 用户在弹窗前犹豫多久都不会弹出第二个原生框。旧稿的单段 3 s 超时把「前端无响应」与「用户没点按钮」混为一谈（犹豫超 3 秒就在正常弹窗头上再弹一个原生框），是设计错误，已修正。
11. **再点逃生门**：前端若在 ack 之后死掉，交互线程会挂在 recv、phase 停在 Intercepting，X/托盘的所有后续点击都被 CAS 吞掉 —— 应用从此无法退出。所以再点 X/托盘必须置 `reclick`，心跳发现后先 re-emit 验证（前端活着会再 ack 一次、**不**弹原生框），3 s 确无回音才降级原生对话框，保证「弹窗路径」永远走得通。

### 6.4 关于系统关机钩子的平台现状（2026-09-27 源码级核验，证据分级标注）

- **Windows 事件链（逐字证据）**：tao 收 `WM_ENDSESSION(TRUE)` 调用 `loop_destroyed()`，它**同步派发** `Event::LoopDestroyed` —— tao `src/platform_impl/windows/event_loop/runner.rs`：`loop_destroyed()` → `move_state_to(Destroyed)`，状态机 `(Idle, Destroyed) => call_event_handler(Event::LoopDestroyed)`，直接同步调用注册闭包。tauri 侧把 `LoopDestroyed` 映射为 `RunEvent::Exit`（证据链完整；唯这一行映射因 `tauri-runtime-wry/src/lib.rs` 超大未能逐字取回，**标注：未确认（字面）**）。该路径**不发 `RunEvent::ExitRequested`**（它只挂在 `app.exit()` 的请求退出通道上）→ ExitRequested 里既有的 `persist_before_exit`（安全模式布局回写）在关机路径被跳过，作为已知损失如实承认。
- **版本矩阵（决定行为差异，crates.io / releases 已核验）**：PR [tauri-apps/tao#1157](https://github.com/tauri-apps/tao/pull/1157)（同步派发后再 `std::process::exit(0)`，修复 Restart Manager 二次消息触发的 "cannot move state from Destroyed" panic）**首发于 tao 0.37.0**（2026-08-21 发布）；tauri 2.11.x 钉 `tao ^0.35` —— **本仓库当前锁（tauri 2.11.x + tao 0.35.3）是旧行为**：同步派发后只 `return LRESULT(0)`，事件循环自然退出、`.run()` 返回、进程正常结束。将来依赖重解析到 tauri 2.12.0（依赖 `tao ^0.37.0`，2026-09-26 发布）才会带上 `exit(0)` 新行为。**两个版本下 `RunEvent::Exit` 都在进程死亡前同步派发** → `cleanup_sync` 与兜底 sidecar 写（§9.1）都有机会执行（尽力而为）。已知问题：当前锁 tao 0.35.3 在 Restart Manager 场景收第二条 `WM_ENDSESSION` 会 panic（[tauri#15933](https://github.com/tauri-apps/tauri/issues/15933)），上游 0.37.0 已修 —— 实施本机制时可顺带评估把依赖下限提到 tauri ≥2.12.0（`Cargo.toml` 现约束 `>=2.11.1, <3` 允许，重解析锁文件 + CI 回归即可）。
- **时间预算（MS 文档逐字）**：`WM_QUERYENDSESSION` 要求立即返回 TRUE/FALSE、清理应推迟到 `WM_ENDSESSION`；约 5 秒后系统会显示「阻止关机的应用」列表并允许用户强制终止（[WM_QUERYENDSESSION](https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession)）。`WaitToKillAppTimeout` 默认 5000ms 为通说值（官方注册表页未取到，标注：未确认）。`cleanup_sync` 最坏情况（taskkill + ≤3 s 回收）在预算内但余量不大 —— 这就是关机路径不做任何 UI、不做长等待的依据。
- **设计立场（不变）**：上述事件链属于 tao/tauri 内部实现细节，任何版本都可能改（#1157 即为实例）—— 因此孤儿防护**不依赖任何用户态回调**，Job Object `KILL_ON_JOB_CLOSE`（壳进程亡 → 内核关句柄 → 整树歼灭，§7.2）是唯一硬保证；`RunEvent::Exit` 的 cleanup_sync 与兜底 sidecar 写只是尽力而为的加分项。
- 若未来要把关机时的案发落盘从「尽力而为」升级为「可靠」，或给 DSH 优雅通知，唯一可靠挂点是 message-only 窗口子类化、在 `DefWindowProc` 之前截获 `WM_QUERYENDSESSION`（query 阶段允许同步做少量工作，返回 TRUE 后才会收到 `WM_ENDSESSION`）—— `windows-sys` 的 `Win32_UI_WindowsAndMessaging` feature 已启用，属可选增强、不在骨架范围；`request_exit(System)`（§5.2，零 UI + 1.5 s 短预算）就是为它预留的统一入口。
- **macOS**：需 `applicationShouldTerminate`（objc2 delegate），Tauri 2 未公开；本程序当前仅 Windows 打包，Unix 路径按 §7.4 的进程组方案前向移植即可。
- **Linux**：logind `PrepareForShutdown` DBus 信号，可选增强。

---

## 7. 跨平台进程树清理（输出要求 3）

### 7.1 `shutdown_dsh_tree`（process.rs 新增，替代退出路径上的直接强杀）

```rust
/// 退出路径的完整清理：优雅终止 → 超时强杀整树 → 回收。
/// 与 stop_internal（停止按钮：立即 taskkill）不同，这是退出专用：
/// 先给 DSH 一个保存现场的机会，超时绝不留情。
pub(crate) fn shutdown_dsh_tree(app: &AppHandle, graceful_wait: Duration) {
    let state = app.state::<AppState>();
    let pid = *state.pid.lock().unwrap();
    let mut child = state.child.lock().unwrap().take();

    // ---- 1. 优雅终止 ----
    if let Some(p) = pid {
        graceful_signal(app, p);
    }
    if let Some(c) = child.as_mut() {
        let deadline = Instant::now() + graceful_wait;
        while Instant::now() < deadline {
            match c.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    // ---- 2. 超时强杀：绝不只杀父进程 ----
    #[cfg(windows)]
    {
        let job_alive = child.as_mut()
            .map(|c| matches!(c.try_wait(), Ok(None)))
            .unwrap_or(false);
        if job_alive {
            if let Some(job) = state.job.lock().unwrap().as_ref() {
                if win::terminate_job(job) {
                    process_log_debug(app, "TerminateJobObject ok");
                }
            }
        }
    }
    #[cfg(unix)]
    if let Some(p) = pid {
        // spawn 时 process_group(0) → pgid == pid；SIGKILL 整组，不留孤儿
        let _ = unix::killpg_force(p);
    }
    // 降级兜底（Job 句柄缺失 / OpenProcess 失败等异常路径）：既有 taskkill /T /F
    let still_alive = child.as_mut()
        .map(|c| matches!(c.try_wait(), Ok(None)))
        .unwrap_or(false);
    if still_alive {
        if let Some(p) = pid {
            let _ = run_taskkill(p);
        }
    }

    // ---- 3. 回收句柄 + 收尾 ----
    if let Some(mut c) = child {
        let _ = c.wait();
    }
    state.close_jobs();            // 关 Job 句柄（KILL_ON_JOB_CLOSE 二次保险）
    crate::safe::cleanup_safe_sync(app);   // 安全模式实例同样处理（进程级操作，线程安全；
                                           // 若日后在其中新增窗口操作，必须 run_on_main_thread 投递）
    // 与 stop_internal 的差异：**不调** destroy_dsh_webview / set_status ——
    // 那两个是「停止按钮」的 UI 收尾，需要主线程；本函数按 §5.5 线程契约跑在
    // helper 线程上，且窗口马上随 app.exit(0) 销毁，无需再修饰。
}

/// 优雅终止的跨平台策略
fn graceful_signal(app: &AppHandle, pid: u32) {
    #[cfg(unix)]
    {
        let _ = unix::killpg_term(pid);   // SIGTERM → node 的默认处理器/DSH 自己的 SIGTERM 钩子
    }
    #[cfg(windows)]
    {
        // Windows 控制台进程没有可靠的 SIGTERM 等价物（详见 §7.3）：
        // 首选「优雅关闭 IPC」——DSH 侧契约端点（§11），一次非阻塞尝试，失败即忽略。
        // 地址与鉴权复用轮询器的 dsh_endpoint_of：detected_url（实际监听地址，
        // 含 launch token）优先，config.port 兜底 —— 别拿 config.port 当真值。
        let (port, token) = crate::tasks::dsh_endpoint_of(app);
        let _ = http_post_shutdown(port, token.as_deref());   // POST /api/desktop/shutdown，800ms 超时
    }
}
```

### 7.2 Windows：Job Object（主）+ taskkill（兜底）

现状与增量：

- **spawn 时**（已有，`start_internal:1576` / `safe.rs:643`）：`CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + `AssignProcessToJobObject`。dsh.cmd → cmd.exe → node.exe → 任意孙进程**全部进 Job**（未给 breakaway 权限，node 无法逃逸）。
- **退出时增量**：`TerminateJobObject(job, 0)` 显式强杀（比依赖 close 句柄的隐式语义更明确、可拿到 BOOL 结果）：

```rust
// process.rs::win 增量（JobObjects feature 已启用，无新依赖）
pub fn terminate_job(h: &JobHandle) -> bool {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    unsafe { TerminateJobObject(h.0, 0) != 0 }
}
```

- **兜底**（已有）：`taskkill /PID <pid> /T /F`（`run_taskkill`）。**绝不用 `/IM`**（现有注释已强调）。
- **崩溃/断电**：进程终止 → 内核关闭 Job 句柄 → `KILL_ON_JOB_CLOSE` 杀整树。这就是「系统强杀也不留孤儿」的最终保险，也是为什么系统关机路径不需要任何额外代码。
- 为什么**不需要** `shared_child`：它的价值是跨线程共享 `Child` 所有权/等待句柄；本设计里 Child 只有 AppState 一个所有者，`try_wait` 轮询已够。引入它徒增依赖。

### 7.3 Windows「优雅终止」的真实约束（为什么首选 HTTP 而不是 Ctrl 事件)

- `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, <pgid>)` 只对**共享同一控制台**且以 `CREATE_NEW_PROCESS_GROUP` 创建的进程组生效。本程序以 `CREATE_NO_WINDOW` spawn，每个子进程有独立控制台；父进程要 `AttachConsole(child_pid)` → `SetConsoleCtrlHandler(NULL, TRUE)`（忽略自己收到的同一个事件）→ 发送 → `FreeConsole`。此路径能通但竞态面大（挂接/拆离窗口期内本进程其他线程的子进程也进新控制台、ctrl 处理器全局状态被污染）。
- 结论：**优雅终止 = DSH 的 HTTP 关闭端点（一次尝试，800ms 超时）**；不通就直接进强杀阶段。Windows 上牺牲 0~3 s 的优雅窗口换取确定性，比引入不可靠的信号模拟更符合本仓库「宁可报清楚，不要静默错」的风格。

### 7.4 Unix/macOS：进程组（前向移植设计）

```rust
// Cargo.toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"          // tauri 在 unix 目标本就传递依赖，锁文件已有

// process.rs
#[cfg(unix)]
pub(crate) mod unix {
    /// spawn 侧：子进程独立进程组（pgid = 子 pid）。
    /// std::os::unix::process::CommandExt::process_group（Rust 1.64+，MSRV 1.77.2 ✓），
    /// 优于 unsafe pre_exec + libc::setsid：无需 fork 中间态，语义等价。
    pub fn apply_process_group(cmd: &mut std::process::Command) {
        let _ = cmd.process_group(0);
    }
    fn killpg(pgid: u32, sig: i32) -> bool {
        unsafe { libc::killpg(pgid as libc::pid_t, sig) == 0 }
    }
    pub fn killpg_term(pgid: u32) -> bool { killpg(pgid, libc::SIGTERM) }
    pub fn killpg_force(pgid: u32) -> bool { killpg(pgid, libc::SIGKILL) }
}

// start_internal 的 spawn 前补一行：
#[cfg(unix)]
unix::apply_process_group(&mut cmd);
```

- 优雅：`killpg(pid, SIGTERM)` → node/DSH 的 SIGTERM 钩子有机会落盘；3 s 后 `killpg(pid, SIGKILL)` 整组歼灭。`killpg` 而非逐个 kill，正是「绝不留下 Python/Shell 孤儿」的 POSIX 答案。
- 崩溃兜底（Unix 没有 Job Object）：可选在子进程 pre_exec 里 `prctl(PR_SET_PDEATHSIG, SIGKILL)`，让 DSH 在壳被强杀时由内核补刀；macOS 无 prctl，靠 launchd/用户手动，当前不影响 Windows 主线。

---

## 8. 全局退出锁的并发原语选型（输出要求 4）

**结论：`std::sync::Mutex<BTreeMap>` + `AtomicU8`（阶段）+ `AtomicBool`（标志位），不引入 tokio。**

论证：

1. **本 crate 没有 async。** 全部后台流（`start_internal` 的 autostart 延迟、`wait_ready_and_embed`、`spawn_log_reader`、安全模式线程）都是 `std::thread`。`tokio::sync::RwLock`/`Mutex` 的核心价值是「可以跨 `.await` 持锁、锁内部用任务通知代替线程阻塞」—— 本设计临界区内没有 await 点，tokio 锁的一个好处都用不上，却要为此引入整个 tokio 依赖树。
2. **`RwLock` 也不值得。** 读操作（弹窗 1 Hz 轮询 snapshot、退出前 snapshot）与写操作（任务注册/完成/切片替换）量级相同、每次都是亚微秒级的小 Map 操作。RwLock 换来的是更高的内存屏障成本、写者饥饿风险与更复杂的锁升级问题；std 文档自身的指引就是「除非剖析证明读争用是瓶颈，否则用 Mutex」。
3. **阶段闸用 `AtomicU8` 而不是塞进 Mutex**：`try_begin_intercept`/`try_begin_exiting`/`cancel_intercept` 是这个「锁」真正的语义（互斥进入、唯一退出执行者），`compare_exchange` 一条指令完成，**不碰任务表锁** —— 快路径零争用，且阶段判定与数据访问解耦。
4. **锁纪律（写成规矩，防止后人踩）**：
   - 永不持有 Mutex 跨 `emit` / `invoke` / `sleep` / HTTP 调用（本设计所有长等待都在无锁区）；
   - 锁序单向：TaskRegistry 持锁期间绝不调用会拿 `AppState` 锁的代码（需要跨状态时，先在 TaskRegistry 锁内把数据 clone 出来，释放后再动 AppState）—— 反之 AppState 侧也永不回调 TaskRegistry，双方无环即无死锁；
   - 退出路径上的锁全部 `.lock().unwrap()`（与 AppState/SafeState 现状一致）：退出途中 Mutex 中毒意味着已有 panic，此时脏数据也要好过第二次 panic 吞掉退出流程；若要更稳，用 `unwrap_or_else(|e| e.into_inner())`（config.rs 测试模块同款）。

---

## 9. 案发现场保留与启动通知（需求 5）

### 9.1 sidecar 文件：`<config_dir>\exit-state.json`

不复用 config.json：它不是用户设置，是运行时事故记录；单键读写 `write_config_key` 的读取-改写-回写模式在这里反而增加半途损坏面。复用 `config::write_atomic`（L-6）保证永不半截。

```rust
#[derive(Serialize, Deserialize)]
struct ExitState {
    /// "normal" | "forced" | "system"
    exit_kind: String,
    /// 被中断任务的 {id, kind, title, started_at_ms}
    killed_tasks: Vec<KilledTask>,
    at_ms: u64,
}

fn persist_exit_state(app: &AppHandle, kind: ExitKind, tasks: Vec<TaskSnapshot>) {
    let dir = config::config_dir(app);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("exit-state.json");
    let state = ExitState {
        exit_kind: kind_label(kind).to_string(),
        killed_tasks: tasks.into_iter().map(KilledTask::from).collect(),
        at_ms: now_ms(),
    };
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        let _ = config::write_atomic(&path, text.as_bytes()); // 失败仅记日志，不阻断退出
    }
}
```

- **强制退出**：`perform_exit(Forced)` 在杀进程**之前**写 → 磁盘上一定有记录。
- **系统关机**：不经过 `perform_exit`（`request_exit(System)` 未接线），但有 **`RunEvent::Exit` 兜底写**——§6.4 已核验：`LoopDestroyed` 在进程死亡前**同步派发**（当前锁 tao 0.35.3 与未来 tao 0.37+ 皆然），`RunEvent::Exit` 尽力执行。兜底逻辑：Exit 处理器在 `cleanup_sync` 之前检查 `exit-state.json` 是否存在，不存在（= 没经过 perform_exit）就补写一条 `exit_kind:"system"` + 当前快照。若 OS 在 ~5 s 预算内先强杀，该记录可能缺失 —— 与断电同等对待，孤儿防护仍由 Job Object 硬保证。`WM_QUERYENDSESSION` 增强可把它从「尽力而为」升级为「可靠」，本期不做。
- **异常断电/被 OS 无预警强杀**：来不及写 —— 如实承认这是限制；此场景用户损失由 Job Object（无孤儿）与 DSH 自身的原子写/WAL 兜底，桌面端无法在断电瞬间做任何事。可选增强：任务注册成功时即追加一条轻量 journal，换取断电也留痕（代价是每次任务启停都写盘，默认不做）。
- **正常退出（无任务）**：也写一份 `exit_kind:"normal"` 空列表，把上一轮的记录清掉，避免「上上次」的陈旧报告在新会话里复活。

`RunEvent::Exit` 兜底写骨架（§5.3 接线③调用；Exit 回调在主线程，纯文件操作、微秒级）：

```rust
/// 关机路径的兜底落盘：perform_exit 没跑过（exit-state.json 不存在）时，
/// 补写 exit_kind:"system" + 当前快照。perform_exit 已写过（正常/强退，
/// 含「正常退出也写空列表」的约定）则 no-op；主写因磁盘错误失败时恰好免费重试一次。
pub fn persist_exit_state_fallback(app: &AppHandle) {
    let path = config::config_dir(app).join("exit-state.json");
    if path.exists() { return; }
    let tasks = app.state::<TaskRegistry>().snapshot();
    persist_exit_state(app, ExitKind::System, tasks);
}
```

### 9.2 下次启动的消费

```rust
/// 命令：启动后前端调用一次。有事故记录 → 返回并删除文件（消费即清，防止每次启动重复报）
#[tauri::command]
pub fn get_last_session_killed(app: AppHandle) -> Option<KilledSessionReport> {
    let path = config::config_dir(&app).join("exit-state.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let state: ExitState = serde_json::from_str(&text).ok()?;
    if state.killed_tasks.is_empty() && state.exit_kind == "normal" {
        return None;
    }
    Some(KilledSessionReport { exit_kind: state.exit_kind, tasks: state.killed_tasks })
}
```

通知呈现（两条路，主推第一条）：

1. **应用内横幅/toast（默认，零依赖）**：`main.js` init 里 `invoke('get_last_session_killed')`，非空则 toast/横幅「上次有 X 个任务被意外中断，点击查看详情」，点击打开详情弹窗（逐条列出 kind/title/中断时刻）。窗口开机自启时是隐藏的 —— 横幅在窗口首次显示时再渲染（`visibilitychange`/show 时检查一次标志位）。
2. **系统级原生通知（可选增强）**：`tauri-plugin-notification`（官方）。需要：Cargo.toml 增依赖 + `capabilities/default.json` 增 `"notification:default"` 权限 + `app.notification().builder().title(..).body(..).show()`。仅当希望「开机自启、窗口未开」时也弹系统气泡时才值得加。

---

## 10. 前端骨架（index.html / main.js / i18n）

### 10.1 `index.html`（与 safe-verify-modal 同款结构）

```html
<div id="exit-confirm-modal" class="modal hidden">
  <div class="modal-box narrow">
    <div class="modal-title" data-i18n="exit_modal_title">有任务正在运行</div>
    <div class="safe-fact" data-i18n="exit_modal_desc">现在退出会中断以下任务；也可以先把窗口藏到托盘，让任务继续。</div>
    <div id="exit-task-list" class="exit-task-list"></div>
    <div class="modal-actions">
      <button id="btn-exit-keep" class="btn" data-i18n="exit_btn_keep">继续后台运行</button>
      <button id="btn-exit-force" class="btn primary" data-i18n="exit_btn_force">强制退出</button>
    </div>
  </div>
</div>
```

### 10.2 `main.js`

```js
// 1) MODALS 数组加入 'exit-confirm-modal' —— 触发既有「弹窗打开即隐藏内嵌 DSH webview」机制
const MODALS = ['settings-modal', 'log-modal', 'update-modal', 'safe-modal', 'safe-verify-modal', 'exit-confirm-modal'];

// 2) 监听与轮询
let exitListTimer = null;

function renderExitTaskRow(t) {
  const row = document.createElement('div');
  row.className = 'exit-task-row' + (t.abort_on_exit ? ' abort' : '');
  const title = document.createElement('span');
  title.className = 'exit-task-title';
  title.textContent = t.title;                        // 安全规约：动态文本一律 textContent
  const meta = document.createElement('span');
  meta.className = 'exit-task-meta';
  meta.textContent = `${t('exit_kind_' + t.kind)} · ${fmtElapsed(t.started_at_ms)}`
    + (t.abort_on_exit ? ` · ${t('exit_will_abort')}` : '');
  row.append(title, meta);
  return row;
}

async function renderExitTasks() {
  const tasks = await invoke('get_active_tasks').catch(() => []);
  const box = $('exit-task-list');
  box.textContent = '';
  for (const task of tasks) box.append(renderExitTaskRow(task));
  if (!tasks.length) {
    const empty = document.createElement('div');
    empty.className = 'exit-task-empty';
    empty.textContent = t('exit_all_done');           // 「全部任务已完成，可以安全退出」
    box.append(empty);
    $('btn-exit-force').textContent = t('exit_btn_quit_now'); // 按钮语义降级为「退出」
  }
}

function fmtElapsed(startedAtMs) {
  const s = Math.max(0, Math.floor((Date.now() - startedAtMs) / 1000));
  return s < 60 ? t('exit_elapsed_sec', s) : t('exit_elapsed_min', Math.floor(s / 60), s % 60);
}

// init() 的 listen 区追加：
await listen('exit-confirm-show', (e) => {
  showModal('exit-confirm-modal');                    // 内含 syncWebviewVisibility（隐藏 DSH 页）
  renderExitTasks();
  exitListTimer = setInterval(renderExitTasks, 1000); // 需求：实时刷新，完成即从列表消失
  // 渲染完成回执（两段式握手第一段，§5.4）：Rust 侧以此区分「前端活着、用户没点」
  // 与「前端无响应」。必须在弹窗显示后发送；重复收到事件（再点逃生门的 re-emit）
  // 会重走本监听器 → 重新 ack，语义幂等。
  invoke('exit_decision', { decision: 'ack' }).catch(() => {});
});
await listen('exit-confirm-hide', () => {
  clearInterval(exitListTimer); exitListTimer = null;
  hideModal('exit-confirm-modal');                    // 内含恢复 DSH webview 可见性
});
await listen('exit-registry-unlocked', () => {});     // 预留：前端可借此恢复被禁用的启动按钮

// bindUI() 追加：
$('btn-exit-keep').onclick = () => invoke('exit_decision', { decision: 'keep' }).catch(() => {});
$('btn-exit-force').onclick = () => invoke('exit_decision', { decision: 'force' }).catch(() => {});

// init()/postInit() 启动横幅（§9.2 路线 1）：
const lastKill = await invoke('get_last_session_killed').catch(() => null);
if (lastKill && lastKill.tasks.length) {
  toast(t('notify_killed_body', lastKill.tasks.length), true);
  // 可选：点开详情弹窗，逐条列 lastKill.tasks（kind/title/时刻）
}
```

### 10.3 i18n 新键（zh / en，加进 `i18n.rs` 与 `src/i18n.js`）

| key | zh | en |
|---|---|---|
| `exit_modal_title` | 有任务正在运行 | Tasks are still running |
| `exit_modal_desc` | 现在退出会中断以下任务；也可以先把窗口藏到托盘，让任务继续。 | Quitting now interrupts the tasks below. You can hide the window to the tray and let them finish. |
| `exit_task_kind_agent` | Agent 任务 | Agent task |
| `exit_task_kind_terminal` | 终端命令 | Terminal command |
| `exit_task_kind_file_write` | 文件写入中 | Writing files |
| `exit_task_kind_update` | 更新 DSH | Updating DSH |
| `exit_task_kind_install` | 引导安装 | Setup install |
| `exit_btn_keep` | 继续后台运行 | Keep running in background |
| `exit_btn_force` | 强制退出 | Force quit |
| `exit_btn_quit_now` | 退出 | Quit |
| `exit_will_abort` | 退出时将中断 | Will be interrupted on exit |
| `exit_all_done` | 全部任务已完成，可以安全退出 | All tasks finished — safe to quit |
| `exit_elapsed_sec` | 已运行 {0} 秒 | running for {0} s |
| `exit_elapsed_min` | 已运行 {0} 分 {1} 秒 | running for {0} m {1} s |
| `exit_fallback_title` | 确认退出 DSH Desktop | Quit DSH Desktop? |
| `exit_fallback_body` | 以下任务仍在运行，强制退出会中断它们： | These tasks are still running and will be interrupted: |
| `exit_fallback_none` | 界面无响应，无法显示任务列表。仍要强制退出吗？ | The UI is unresponsive, so the task list cannot be shown. Force quit anyway? |
| `notify_killed_title` | 上次意外中断 | Interrupted last session |
| `notify_killed_body` | 上次有 {0} 个任务被意外中断 | {0} task(s) were interrupted last time |
| `log_exit_kind` | [launcher] 退出流程开始（类型 {0}） | [launcher] Exit flow started (kind: {0}) |

---

## 11. DSH 侧协作契约（需与 DSH 仓库对齐，桌面端已做全降级）

| 契约 | 请求 | 响应 | 缺失时桌面端行为 |
|---|---|---|---|
| 任务快照 | `GET /api/desktop/tasks?token=<launch token>` | `200 {"tasks":[{"id":"…","kind":"agent\|terminal\|file_write","title":"生成周报 PPT","started_at_ms":1758945600000}]}`；`id` 为 DSH 侧稳定标识（session/turn/job id，壳以它为切片键），任务结束应从列表消失，`started_at_ms` 以 DSH 为权威 | §1.3 fail-safe 三分支：404/401 → 清空切片 + 停止探测；超时/5xx → **保留旧切片** |
| 优雅关闭 | `POST /api/desktop/shutdown?token=<launch token>` | 202 即可（回包时机不重要，壳以**进程退出**为准：`try_wait` 轮询至 `GRACEFUL_TIMEOUT`）；DSH 内部建议映射到 `application.shutdown.shutdown(0)`，自行完成会话落盘后退出 | 404/超时 → 直接进强杀阶段（Windows 本就无可靠信号，§7.3） |

**契约落地说明（给 DSH 侧，含上游调研结论）**：
- **判据可直接照搬上游**：官方 Electron 壳的 `apps/desktop-host/src/quit-inspection.ts`（`hasDesktopActiveTasks`）已定义「活跃任务」——任一 agent `status==='running'` 或 `inbox.nextTurn/nextStep` 非空、任一 job status running/stopping；schedule 任务上游单独回 `scheduledTasks` 字段，是否并入、并入哪个 kind 由契约评审定。REST 端点只是把同一判据对外暴露，逻辑上游已在产线验证。
- **鉴权用 launch token（拍板，不做回环豁免）**：DSH 的 `/api` 前缀有浏览器信任栅栏与 `?token=` 会话令牌（`dsh-client-connection`）。这两个端点校验同一个 launch token——壳侧已经持有它（DSH 输出行 `dsh web: http://127.0.0.1:<port>/?token=…` 已被解析进 `AppState.detected_url`，`process.rs:594-622`）。不因为「是本机回环」就放开令牌豁免面。
- **不做「冻结/拒新任务」端点**：需求约束 4 的「拒绝创建新 Agent 任务」在壳侧无法达成（任务在 DSH 页面里创建），上游的 503 准入锁是 Host 进程内中间件、外部不可搬。本期语义收敛为：Intercepting 期间新出现的任务标 `abort_on_exit` 并在弹窗中实时可见（§4）；将来确有需求再议。

---

## 12. 已知边界与测试清单

### 12.1 边界（如实声明）

1. **`running-external`（连接现有服务）不参与任何拦截**：那不是我们的进程树，`stop_internal` 本就拒绝杀它（现有日志文案同语义）；轮询器也只在 status == "running" 时查询任务（§4.1）。
2. **提权的 msiexec（Node 引导安装）杀不动**：非提权的壳进程无法终止提权安装事务，中途强杀 MSI 也有风险。设计上引导安装任务在弹窗里出现（用户会看到「引导安装」在跑），【强制退出】对它只记录不追杀，安装器随 OS 策略自行收尾；此语义写在 KilledTask 的 kind 上，详情弹窗如实呈现。
3. **DSH 内部任务的真实性依赖 §11 契约落地**；契约未落地前，本机制对「Agent 任务」的可见性为零 —— 此时覆盖的只有更新 DSH、引导安装两类壳内长任务（§1.3 拍板：DSH 服务进程本身不参与拦截），纯增量收益、零骚扰。
4. **断电场景**无案发报告；**系统关机**有 `RunEvent::Exit` 尽力而为的兜底写（§9.1；已核验该回调在进程死亡前同步派发，但 OS ~5 s 预算内被强杀时仍可能缺失）；两者的孤儿防护都交给 Job Object。
5. **安全模式实例（3081）是注册表盲区**：轮询器只查询日常实例；安全实例的进程清理由既有 `cleanup_safe_sync` 兜底（`lib.rs:50-51`），其任务可见性列为后续项，本期不做。
6. **上游 apps/desktop 的其余特色明确不纳入本期**（已调研，以后再考虑）：首次藏托盘确认横幅（background-notice）、每次退出必弹确认框、update-journal、crash-report。

### 12.2 手工/自动测试清单

- [ ] `close_action=tray`：点 X 藏托盘，无任何弹窗（回归）。
- [ ] `close_action=quit` + 无任务：点 X / 托盘退出，行为与旧版一致（直退；窗口立即隐藏，10 s 清理期间不出现「(未响应)」幽灵标题）。
- [ ] `close_action=quit` + 更新 DSH 进行中：点 X → 弹窗列出「更新 DSH」，实时进度；【继续后台运行】→ 窗口藏托盘、npm 不中断、托盘恢复窗口；再次退出 → 仍拦截。
- [ ] 强制退出：DSH 停止 → `exit-state.json` 生成 → 重启后 toast 报告 N 个任务中断。
- [ ] WebView 无响应降级：临时注释掉前端监听（不发 ack）→ 3 秒后弹原生对话框，两枚按钮文案正确、选择生效。
- [ ] 两段式握手：前端正常渲染但用户 10 秒后才点按钮 → 期间**不**出现原生对话框（ack 已收到，无限期等待）。
- [ ] 再点逃生门：ack 后让前端死掉（任务管理器结束 WebView2 渲染进程模拟）→ 再点 X → re-emit 验证无回音后 ≤8 s 弹原生框；前端活着时再点 X → 只是幂等重弹 web 弹窗，**不**出原生框。
- [ ] fail-safe 切片：DSH 活着但 `/api/desktop/tasks` 超时（防火墙丢包模拟）→ 保留旧切片 → 退出仍拦截；端点 404 → 切片清空且停止探测（日志仅一条）。
- [ ] DSH 切片稳定性：同一任务跨两轮轮询 → id 不变、已运行时长真实累计、列表不跳动；弹窗期间 DSH 冒出新任务 → 仅新任务带「退出时将中断」标记，原有任务不带。
- [ ] 系统关机（Windows「关机」）：应用随系统退出，无弹窗；DSH 无孤儿残留（`tasklist`/端口检查）；`exit-state.json` 应出现 `exit_kind:"system"` 兜底记录（RunEvent::Exit 兜底写，§9.1）；若被 OS 在预算内强杀导致缺失，不算失败（Job Object 仍无孤儿）。
- [ ] 弹窗打开期间从设置页触发新安装任务 → 被拒绝（日志可见）。
- [ ] 连点 X ×3、点 X 后立刻点托盘退出 → 只有一个流程（日志仅一条 `log_exit_kind`）。
- [ ] `cargo test`：TaskRegistry 阶段机并发用例（N 线程同时 try_begin_intercept / try_begin_exiting 恰一个成功）；replace_dsh_slice 的 abort_on_exit 只标 Intercepting 期间新增 id；DshFetch 响应分类（200/404/401/超时/5xx → 三分支）为纯函数可测。
- [ ] 杀壳进程（模拟崩溃）：Job Object 兜底，DSH 树消亡，无 3080 占留。
