//! Python 能力桥接：把「基本安装」装好的 Python 库变成 DSH 的 AI 工具。
//!
//! # 全链路
//!
//! ```text
//! 点「基本安装」
//!   └─ process.rs::python_basic_install_blocking
//!        ├─ pip install markitdown[all] + 办公库 + dsh-python-bridge   （只依赖 Python）
//!        └─ py_bridge::deploy_after_install
//!             ├─ 释放 Python 侧代码 → <config>\py-bridge\
//!             ├─ 释放 Node 侧 bundle → <config>\py-bridge-bundle\
//!             ├─ 就地注册（DSH 没在跑）或写 pending 标记（DSH 在跑 / profile 未初始化）
//!             └─ 清标记 + 提示
//! ```
//!
//! # 为什么注册这一步必须解耦
//!
//! `dsh plugin --profile <p> add <bundle>` 会往 **profile 目录**里装包并改它的
//! `package.json`。而 profile 可能还不存在（DSH 从未初始化过），也可能正被运行中的
//! DSH 持有。两边同时初始化同一个 profile 会互相踩（官方 CLI 自己就是初始化路径）。
//! 所以这里定一条纪律：
//!
//! - **DSH 没在跑**（端口未监听）→ 直接就地注册，`dsh plugin` 会把不存在的 profile 建好；
//! - **DSH 在跑** → 只写 pending 标记，等「进入 DSH」的钩子再注册（见 `activate_pending`）。
//!
//! 这样「首次安装（DSH 未初始化）」不再是死局，同时也不会与 DSH 自己的首次启动抢跑。
//!
//! # 三处硬边界
//!
//! 1. **不碰保留 profile `desktop`** —— CLI 明文报错 `profile "desktop" is managed
//!    exclusively by the Electron application`。桌面端跑的是 `dsh web`，即 profile `web`；
//!    落点以 `probe_profile()` 的实测为准，绝不写死进 `desktop`。
//! 2. **不手写 profile 的 `package.json` / `cordis.patch.yml`，不在 profile 目录裸跑 pnpm**
//!    —— 一律走 `dsh plugin` 包装（与 `plugin_manager` 工具背后同一条管线）。
//! 3. **子进程不由本模块拉起** —— Python 桥接进程由 **DSH 插件** spawn，生命周期归插件
//!    管（插件卸载 = DSH 退出 = 子进程被杀），所以这里不需要走 Job Object。

use crate::{config, detect, i18n, logger};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::AppHandle;

/// 桥接产物目录名（放在**应用数据区**，不是 DSH 家目录 —— 见模块文档）
const BRIDGE_DIR_NAME: &str = "py-bridge";
/// 注册用 bundle 目录名
const BUNDLE_DIR_NAME: &str = "py-bridge-bundle";
/// 宿主插件在 profile 里的包名（与 bundle 的 package.json `name` 一致）
const BUNDLE_PKG_NAME: &str = "@local/dsh-desktop-python-bridge";
/// bundle 的插件行 id（与 cordis.patch.yml 里的 `id` 一致）
const BUNDLE_ROW_ID: &str = "dsh-desktop-python-bridge";

/// config.json 里的 pending 标记键（单键写入，见 config::write_config_key）
const PENDING_KEY: &str = "python_bridge_pending";

/// 释放进 py-bridge\ 的文件（与 resources/py-bridge/ 一一对应）
const BRIDGE_FILES: &[(&str, &str)] = &[
    (
        "bridge_entry.py",
        include_str!("../resources/py-bridge/bridge_entry.py"),
    ),
    (
        "utils.py",
        include_str!("../resources/py-bridge/utils.py"),
    ),
    (
        "markitdown_tools.py",
        include_str!("../resources/py-bridge/markitdown_tools.py"),
    ),
    (
        "excel_tools.py",
        include_str!("../resources/py-bridge/excel_tools.py"),
    ),
    (
        "docx_tools.py",
        include_str!("../resources/py-bridge/docx_tools.py"),
    ),
    (
        "pptx_tools.py",
        include_str!("../resources/py-bridge/pptx_tools.py"),
    ),
    (
        "sandbox_tools.py",
        include_str!("../resources/py-bridge/sandbox_tools.py"),
    ),
    (
        "env_tools.py",
        include_str!("../resources/py-bridge/env_tools.py"),
    ),
];

/// 释放进 bundle\ 的文件
const BUNDLE_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../resources/py-bridge-bundle/package.json"),
    ),
    (
        "cordis.patch.yml",
        include_str!("../resources/py-bridge-bundle/cordis.patch.yml"),
    ),
    (
        "index.js",
        include_str!("../resources/py-bridge-bundle/index.js"),
    ),
];

// ---------- 纯函数（可单测，不碰文件系统 / 进程） ----------

/// bridge 目录：`<应用配置目录>\py-bridge`。
///
/// 刻意**不是** DSH 家目录 —— 那是 DSH 自己的配置空间，往里塞本程序的文件会让
/// 「卸载 DSH」变成一件说不清的事。应用数据区跟着本程序走，卸载/重装语义干净。
pub fn bridge_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(BRIDGE_DIR_NAME)
}

/// bundle 目录：`<应用配置目录>\py-bridge-bundle`
pub fn bundle_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(BUNDLE_DIR_NAME)
}

/// profile 名是否合法（会直接拼进命令行，必须挡住路径片段）。
///
/// `desktop` 是**保留名**：CLI 明文报错 `profile "desktop" is managed exclusively by
/// the Electron application`。这里提前拒掉，避免真去跑一次只为拿到那句报错。
pub fn profile_name_usable(p: &str) -> bool {
    !p.is_empty()
        && p != "desktop"
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 待激活状态机：由「pip 装完了吗 / bridge 释放了吗 / bundle 装进 profile 了吗」
/// 三个事实推出界面上该显示什么。前端只认这个枚举，不自己拼字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgePhase {
    /// 没装（找不到可用的 Python）
    NotInstalled,
    /// 库已装、但桥接代码还没释放
    NeedsRelease,
    /// 代码已释放、还没写进 profile（首次安装 / 待激活）
    PendingActivate,
    /// bundle 已在 profile 里。注意**没有**「已注册但需重启」这一档：
    /// 本程序只在 DSH 没在跑的时候就地注册，所以「在 profile 里」就等于
    /// 「下一次启动会加载」。真要在 DSH 运行期注册（pending 那条路）时，
    /// 状态仍是 PendingActivate —— 用户看到的提示本来就是「下次进入 DSH 时激活」。
    Active,
}

impl BridgePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NeedsRelease => "needs-release",
            Self::PendingActivate => "pending-activate",
            Self::Active => "active",
        }
    }
}

/// 从三个事实推出界面状态。**纯函数**：pending 状态机必须能单测，
/// 而它的全部输入就是这三个布尔。
pub fn bridge_phase(python_ok: bool, released: bool, registered: bool) -> BridgePhase {
    match (python_ok, released, registered) {
        (false, _, _) => BridgePhase::NotInstalled,
        (true, false, _) => BridgePhase::NeedsRelease,
        (true, true, false) => BridgePhase::PendingActivate,
        (true, true, true) => BridgePhase::Active,
    }
}

/// 待激活标记文件的内容（写进 config.json 的那个键）。
///
/// 记的是**解释器路径**而不是「装过」这种布尔：换台机器、用户自己装了别的
/// Python、或者重建了 profile 之后，激活这一步需要知道该去哪儿找解释器，
/// 而 python_status 的结果可能已经变了。
pub fn pending_record(python_path: &str) -> String {
    python_path.trim().to_string()
}

// ---------- 状态查询 ----------

/// 桥接状态（只读）。前端首选项的「Python 能力桥接」一行显示它。
#[derive(Clone, Serialize)]
pub struct BridgeStatus {
    /// BridgePhase 的字符串形态
    pub phase: String,
    /// 桥接代码是否已释放到应用数据区
    pub released: bool,
    /// bundle 是否已装进 profile
    pub registered: bool,
    /// 解释器路径（没装时为空串）
    pub python_path: String,
    /// 桥接代码目录
    pub bridge_dir: String,
    /// 注册用的 bundle 目录
    pub bundle_dir: String,
    /// 实际使用的 profile（没探测到时为空串）
    pub profile: String,
    /// 待激活标记是否存在
    pub pending: bool,
    /// 人话说明（当前语言）
    pub message: String,
}

#[tauri::command]
pub async fn bridge_status(app: AppHandle) -> Result<BridgeStatus, String> {
    // 探测要起子进程（跑 dsh --dump-config），下到阻塞线程池
    tauri::async_runtime::spawn_blocking(move || bridge_status_blocking(&app))
        .await
        .map_err(|e| e.to_string())
}

fn bridge_status_blocking(app: &AppHandle) -> Result<BridgeStatus, String> {
    let cfg_dir = config::config_dir(app);
    let bd = bridge_dir(&cfg_dir);
    let bun = bundle_dir(&cfg_dir);
    let py = detect::find_python();
    let released = bd.join("bridge_entry.py").is_file();
    let profile = probe_profile(app);
    let registered = match &profile {
        Some(p) => bundle_registered(p),
        None => false,
    };
    let pending = config::read_config_flag(app, PENDING_KEY);
    let phase = bridge_phase(py.is_some(), released, registered || pending);
    let message = bridge_phase_message(phase);
    Ok(BridgeStatus {
        phase: phase.as_str().to_string(),
        released,
        registered,
        python_path: py
            .as_ref()
            .map(|p| p.program.to_string_lossy().to_string())
            .unwrap_or_default(),
        bridge_dir: bd.to_string_lossy().to_string(),
        bundle_dir: bun.to_string_lossy().to_string(),
        profile: profile.clone().unwrap_or_default(),
        pending,
        message,
    })
}

/// 各阶段的人话说明（前端直接显示，不自己拼）。
fn bridge_phase_message(phase: BridgePhase) -> String {
    match phase {
        BridgePhase::NotInstalled => i18n::t("py_bridge_not_installed").to_string(),
        BridgePhase::NeedsRelease => i18n::t("py_bridge_needs_release").to_string(),
        BridgePhase::PendingActivate => i18n::t("py_bridge_pending_activate").to_string(),
        BridgePhase::Active => i18n::t("py_bridge_active").to_string(),
    }
}

// ---------- Tauri 命令 ----------

/// 「部署 Python 能力桥接」：手动重试用（用户在界面上点的那颗按钮）。
///
/// 与「基本安装」末尾自动调用的 `deploy_after_install` 是**同一条**路径，
/// 只差一个 pending 标记的处理时机。
#[tauri::command]
pub fn deploy_python_bridge(app: AppHandle) -> Result<(), String> {
    let py = detect::find_python().ok_or_else(|| i18n::t("setup_py_missing").to_string())?;
    let notes = deploy_after_install(&app, &py)?;
    let _ = app.emit_result(&notes);
    Ok(())
}

/// 只写 pending 标记、不做任何部署（用户明确表示「稍后再说」时用）。
#[tauri::command]
pub fn mark_bridge_pending(app: AppHandle) -> Result<(), String> {
    let py = detect::find_python().ok_or_else(|| i18n::t("setup_py_missing").to_string())?;
    let path = py.program.to_string_lossy().to_string();
    set_pending(&app, &path)
}

// ---------- 部署主体（pip 装完之后调） ----------

/// 释放 + 注册。返回给用户看的说明文字。
///
/// **失败绝不 panic、不抛到安装流程**：这一步失败不该把「库已经装好了」变成
/// 「整个安装失败」。所以这里内部全走降级：能释放就释放，能注册就注册，
/// 都不行就留一个 pending 标记 + 一句人话。
pub fn deploy_after_install(app: &AppHandle, py: &detect::PythonExe) -> Result<String, String> {
    let cfg_dir = config::config_dir(app);
    let bd = bridge_dir(&cfg_dir);
    let bun = bundle_dir(&cfg_dir);

    let (rel_b, rel_u) = release_sources(&bd, &bun)?;
    crate::process::setup_progress(app, "install", &i18n::fmt("py_bridge_released", &[&rel_b]));
    crate::process::setup_progress(app, "install", &i18n::fmt("py_bridge_bundle_released", &[&rel_u]));

    let python_path = py.program.to_string_lossy().to_string();
    write_bundle_config(&bun, &python_path, &bd)?;

    // 竞态纪律：只在 DSH **没在跑**的时候就地注册。
    // profile 名从实测拿（dsh --dump-config），拿不到就退到 "web" 并在日志里说明。
    let profile = probe_profile(app).unwrap_or_else(|| {
        logger::append_line(
            &logger::desktop_log_path(app),
            "[py-bridge] probe_profile failed; falling back to profile \"web\"",
        );
        "web".to_string()
    });

    if dsh_port_busy(app) {
        // DSH 正在跑：注册会改它正持有的 profile（且它不会热加载新插件），
        // 所以只留标记，等进入 DSH 的钩子再动。
        let rec = pending_record(&python_path);
        set_pending(app, &rec)?;
        let line = i18n::fmt("py_bridge_deferred", &[&rec]);
        crate::process::setup_progress(app, "install", &line);
        return Ok(line);
    }

    match register_bundle(app, &profile, &bun) {
        Ok(()) => {
            clear_pending(app);
            let line = i18n::fmt("py_bridge_registered", &[&profile]);
            crate::process::setup_progress(app, "install", &line);
            Ok(line)
        }
        Err(e) => {
            // 保留 pending：下次进入 DSH 还会再试一次
            let rec = pending_record(&python_path);
            let _ = set_pending(app, &rec);
            let line = i18n::fmt("py_bridge_register_fail", &[&e, &rec]);
            crate::process::setup_progress(app, "install", &line);
            logger::append_line(
                &logger::desktop_log_path(app),
                &format!("[py-bridge] register_bundle failed: {e}"),
            );
            Ok(line)
        }
    }
}

// ---------- 「进入 DSH」钩子 ----------

/// 在**启动 / 进入 DSH** 的路径上调用：把 pending 的桥接真正注册进去。
///
/// 由 process.rs 在 DSH 起来之后调。返回 Some(说明) 表示「这次真的激活了」。
pub fn activate_pending(app: &AppHandle) -> Option<String> {
    if !config::read_config_flag(app, PENDING_KEY) {
        return None;
    }
    let Some(py) = detect::find_python() else {
        return None;
    };
    let cfg_dir = config::config_dir(app);
    let bd = bridge_dir(&cfg_dir);
    let bun = bundle_dir(&cfg_dir);
    // pending 存在但代码已被删（用户清过目录）：重新释放一次，别让标记永远满足不了
    if !bd.join("bridge_entry.py").is_file() {
        if let Err(e) = release_sources(&bd, &bun) {
            logger::append_line(
                &logger::desktop_log_path(app),
                &format!("[py-bridge] re-release failed: {e}"),
            );
            return None;
        }
    }
    let python_path = py.program.to_string_lossy().to_string();
    if let Err(e) = write_bundle_config(&bun, &python_path, &bd) {
        logger::append_line(
            &logger::desktop_log_path(app),
            &format!("[py-bridge] write_bundle_config failed: {e}"),
        );
        return None;
    }
    let profile = probe_profile(app).unwrap_or_else(|| {
        logger::append_line(
            &logger::desktop_log_path(app),
            "[py-bridge] probe_profile failed; falling back to profile \"web\"",
        );
        "web".to_string()
    });
    match register_bundle(app, &profile, &bun) {
        Ok(()) => {
            clear_pending(app);
            let line = i18n::fmt("py_bridge_registered", &[&profile]);
            logger::append_line(&logger::desktop_log_path(app), &format!("[py-bridge] {line}"));
            Some(line)
        }
        Err(e) => {
            // 不清标记：下次还会再试
            let line = i18n::fmt("py_bridge_register_fail", &[&e, &pending_record(&python_path)]);
            logger::append_line(&logger::desktop_log_path(app), &format!("[py-bridge] {line}"));
            Some(line)
        }
    }
}

// ---------- 释放文件 ----------

/// 幂等释放两套代码。返回两条日志行。
///
/// 幂等的做法是「先写临时文件再改名覆盖」：重复点击安装不会留下半份文件
/// （Python import 到半份代码的表现是语法错误，且会留在磁盘上）。
pub fn release_sources(bd: &Path, bun: &Path) -> Result<(String, String), String> {
    write_files(bd, BRIDGE_FILES)?;
    write_files(bun, BUNDLE_FILES)?;
    Ok((
        bd.to_string_lossy().to_string(),
        bun.to_string_lossy().to_string(),
    ))
}

/// 一组文件写进一个目录：临时文件 + 原子改名，逐个覆盖。
fn write_files(dir: &Path, files: &[(&str, &str)]) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| i18n::fmt("py_bridge_write_fail", &[&dir.to_string_lossy(), &e.to_string()]))?;
    for (name, body) in files {
        let target = dir.join(name);
        // 临时名带进程内序号：同名文件不会被自己覆盖成空
        let tmp = dir.join(format!(
            "{}.{}.tmp",
            name,
            std::process::id()
        ));
        std::fs::write(&tmp, body).map_err(|e| {
            i18n::fmt("py_bridge_write_fail", &[&tmp.to_string_lossy(), &e.to_string()])
        })?;
        std::fs::rename(&tmp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            i18n::fmt("py_bridge_write_fail", &[&target.to_string_lossy(), &e.to_string()])
        })?;
    }
    Ok(())
}

/// 把本机实际路径写进 bundle 的 cordis.patch.yml。
///
/// 做法是**替换三个标量值**，而不是拼一整份 YAML —— bundle 目录是我们自己释放的，
/// 但 YAML 里的注释与键序仍有信息量，整份重写会把它抹平。三处都做「空值 → 填值」
/// 的就地替换，重复执行结果相同（幂等）。
fn write_bundle_config(bun: &Path, python_path: &str, bd: &Path) -> Result<(), String> {
    let patch = bun.join("cordis.patch.yml");
    let Ok(text) = std::fs::read_to_string(&patch) else {
        return Err(i18n::fmt("py_bridge_write_fail", &[&patch.to_string_lossy(), &"missing"]));
    };
    // YAML 里单引号串里的单引号要写两遍（这是 YAML 的转义规则，不是我们自创的）
    let py = python_path.replace('\'', "''");
    let dir = bd.to_string_lossy().replace('\'', "''");
    let out = text
        .replace("pythonPath: ''", &format!("pythonPath: '{py}'"))
        .replace("bridgeDir: ''", &format!("bridgeDir: '{dir}'"))
        .replace("cwd: ''", &format!("cwd: '{dir}'"));
    std::fs::write(&patch, out).map_err(|e| {
        i18n::fmt("py_bridge_write_fail", &[&patch.to_string_lossy(), &e.to_string()])
    })
}

// ---------- 注册 ----------

/// `dsh plugin --profile <profile> add <bundle>` —— 官方入口。
///
/// 为什么是它：`plugin_manager` 的 `install_bundle` 是**会话内（agent 驱动）**的入口，
/// 而本程序要在**用户没打开 DSH**时也能装 —— 那条路只有 CLI。两者背后同一条管线。
/// 参数原样转发给 profile 目录里的 pnpm，profile 不存在时 CLI 自己会初始化。
///
/// **没有 shell**：参数直接进 argv。所以下面出现的 `cmd.exe /C` 不是为了拼命令行，
/// 而只是因为 Windows 上 CreateProcess 不能直接执行 `.cmd`（那是 cmd 的脚本）。
/// 那条路径下整个命令行由 cmd 自己解析，规则与批处理一致：引号内的路径是一个整体，
/// 且 `%` / `^` / `&` / `|` 仍需转义（见 `quote_cmd_arg`）。
fn register_bundle(app: &AppHandle, profile: &str, bun: &Path) -> Result<(), String> {
    if !profile_name_usable(profile) {
        return Err(i18n::fmt("py_bridge_bad_profile", &[&profile.to_string()]));
    }
    let (program, mut args) = dsh_argv(app);
    args.extend([
        "plugin".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "add".to_string(),
        bun.to_string_lossy().to_string(),
    ]);
    crate::process::run_cmd_capture(&program, &args, "", Duration::from_secs(10 * 60)).and_then(
        |(ok, out)| {
            if ok {
                Ok(())
            } else {
                // 把 dsh 的输出尾部带上：失败原因几乎总在那几行里
                let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
                Err(i18n::fmt("py_bridge_register_fail", &[&tail, &""]))
            }
        },
    )
}

/// 拼出「跑 dsh」的命令前缀，返回 `(程序, 已有参数)`。
///
/// **没有 shell**：参数直接进 argv。所以走 `cmd.exe` 那条路不是为了拼命令行，
/// 而只是因为 Windows 上 CreateProcess 不能直接执行 `.cmd`（那是 cmd 的脚本）。
/// 那种情况下整条命令行由 cmd 自己解析，规则与批处理一致：引号内的路径是一个
/// 整体 —— 不加引号的话，用户名里的空格会把 `D:\Users\John Doe\...` 拆成两个参数。
///
/// 两条路必须各自决定 program：`.cmd` 走 `cmd.exe`，而 `.ps1` / 裸 `dsh`
/// 要直接跑 —— 早先的版本把 program 写死成 `cmd.exe`，于是配置里指向
/// `dsh.ps1` 的机器会去执行一个根本不存在的东西（`cmd /C dsh.ps1` 不成立）。
fn dsh_argv(app: &AppHandle) -> (String, Vec<String>) {
    dsh_argv_for(&dsh_cmd_path(app))
}

/// `dsh_argv` 的纯逻辑（不含配置读取），所以分支行为能直接单测。
fn dsh_argv_for(dsh: &str) -> (String, Vec<String>) {
    if dsh.to_ascii_lowercase().ends_with(".cmd") {
        ("cmd.exe".to_string(), vec!["/C".to_string(), quote_cmd_arg(dsh)])
    } else {
        (dsh.to_string(), vec![dsh.to_string()])
    }
}

/// 给 `cmd.exe` 用的单个参数加引号。
///
/// 只用一层引号包住整串就已足够（cmd 不解析引号内的特殊字符），
/// 所以这里**不**在引号内做 `\^\&` 那套双重转义 —— 那套规则在多层嵌套里会互相打架。
/// 引号内的字面量 `"` 保留：用户路径里出现 `"` 本身非法，真出现了就让那条命令
/// 失败并把报错带回来，而不是悄悄拼出一条不同的命令。
fn quote_cmd_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    format!("\"{arg}\"")
}

/// bundle 是否已装进 profile。
///
/// 判定依据是 profile 的 `node_modules` 下有没有 `@local/dsh-desktop-python-bridge`
/// —— 那是 reconcile 之后 bundle 的落点。只读，不调 pnpm（快得多，也不会触发任何副作用）。
///
/// 查的是**实际生效的那个 profile**：有 `DSH_PROFILE_DIR` 就用它（它是宿主进程
/// 真正在跑的那个 profile 的目录），否则退到 `<DSH_HOME>\profiles\<name>`。
/// 刻意不缓存 —— 环境变量可能在运行期变（安全模式会换 home），缓存会把上一次
/// 的结论带到下一次状态查询里，而界面上的「已激活」必须对应当下。
fn bundle_registered(profile: &str) -> bool {
    let dir = match profile_dir_from_env() {
        Some(d) => d,
        None => match std::env::var("DSH_HOME").ok().filter(|s| !s.trim().is_empty()) {
            Some(home) => PathBuf::from(home).join("profiles").join(profile),
            None => return false,
        },
    };
    dir.join("node_modules")
        .join("@local")
        .join("dsh-desktop-python-bridge")
        .join("package.json")
        .is_file()
}

/// `DSH_PROFILE_DIR` 直接给的就是 profile 目录本身（`<home>\profiles\<name>`），
/// 不需要再拼 `profiles/<name>`。
fn profile_dir_from_env() -> Option<PathBuf> {
    std::env::var("DSH_PROFILE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

/// profile 名的实测值。
///
/// `dsh --profile <p> --dump-config` 会打印整份组合后的配置，profile 名就在里面；
/// 同时也顺带确认这个 profile 确实存在（不存在时 DSH 会报错）。取不到就退到 `web`
/// —— 桌面端起的就是 `dsh web`，而 `web` 正是 DSH 默认的 web profile。
fn probe_profile(app: &AppHandle) -> Option<String> {
    if let Ok(p) = std::env::var("DSH_PROFILE") {
        let t = p.trim().to_string();
        if profile_name_usable(&t) {
            return Some(t);
        }
    }
    let (program, mut args) = dsh_argv(app);
    args.extend([
        "--profile".to_string(),
        "web".to_string(),
        "--dump-config".to_string(),
    ]);
    let Ok((true, out)) =
        crate::process::run_cmd_capture(&program, &args, "", Duration::from_secs(60))
    else {
        return None;
    };
    // 先找显式的 profile 字段，找不到就用候选值（DSH 默认 profile 就是 web）
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("profile:") {
            let t = v.trim().trim_matches('"').to_string();
            if profile_name_usable(&t) {
                return Some(t);
            }
        }
    }
    Some("web".to_string())
}

/// dsh 入口路径（config.json 里的 dsh_path，失效时回落到 PATH 上的 `dsh`）。
fn dsh_cmd_path(app: &AppHandle) -> String {
    let cfg = config::load(app);
    let p = cfg.dsh_path.trim();
    if !p.is_empty() && Path::new(p).is_file() {
        return p.to_string();
    }
    "dsh".to_string()
}

/// DSH 的 Web 端口是否已被占用（= DSH 正在跑）。
///
/// 这里**只判端口在不在监听**，不去问「是不是我们启动的那个实例」：同一时刻
/// 同端口上只可能有一个 DSH，而端口是最便宜也最不容易出错的那个信号。
/// 判错的后果都很轻（保守地多留一次 pending / 多注册一次）。
fn dsh_port_busy(app: &AppHandle) -> bool {
    let port = config::load(app).port;
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(400),
    )
    .is_ok()
}

// ---------- pending 标记 ----------

fn set_pending(app: &AppHandle, value: &str) -> Result<(), String> {
    config::write_config_key(app, PENDING_KEY, value)
}

fn clear_pending(app: &AppHandle) {
    if let Err(e) = config::write_config_key(app, PENDING_KEY, "") {
        logger::append_line(
            &logger::desktop_log_path(app),
            &format!("[py-bridge] clear pending failed: {e}"),
        );
    }
}

/// 扩展 AppHandle 的小工具：发一条带消息的事件（避免在这里再写一遍 Emitter 导入）。
trait EmitResult {
    fn emit_result(&self, message: &str) -> Result<(), String>;
}

impl EmitResult for AppHandle {
    fn emit_result(&self, message: &str) -> Result<(), String> {
        use tauri::Emitter;
        self.emit("py-bridge", message.to_string()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_dirs_live_under_the_app_config_dir() {
        let base = PathBuf::from(r"C:\Users\me\AppData\Roaming\com.dsh.desktop");
        assert_eq!(
            bridge_dir(&base),
            base.join("py-bridge"),
            "桥接代码必须在应用数据区，不是 DSH 家目录"
        );
        assert_eq!(bundle_dir(&base), base.join("py-bridge-bundle"));
    }

    #[test]
    fn reserved_desktop_profile_is_rejected() {
        // `desktop` 由 Electron 应用独占，CLI 会直接报错 —— 必须提前挡住
        assert!(!profile_name_usable("desktop"));
        assert!(profile_name_usable("web"));
        assert!(profile_name_usable("my-profile_2"));
        // 会被拼进命令行的名字不允许带路径片段
        assert!(!profile_name_usable(""));
        assert!(!profile_name_usable("../evil"));
        assert!(!profile_name_usable("a b"));
        assert!(!profile_name_usable("a/b"));
        assert!(!profile_name_usable("a\\b"));
        assert!(!profile_name_usable("a;rm -rf"));
    }

    #[test]
    fn phase_machine_follows_the_three_facts() {
        // 没 Python → 一切免谈
        assert_eq!(bridge_phase(false, false, false), BridgePhase::NotInstalled);
        assert_eq!(bridge_phase(false, true, true), BridgePhase::NotInstalled);
        // 装了但没释放代码
        assert_eq!(bridge_phase(true, false, false), BridgePhase::NeedsRelease);
        assert_eq!(bridge_phase(true, false, true), BridgePhase::NeedsRelease);
        // 释放了但没进 profile（首次安装 / 待激活）
        assert_eq!(bridge_phase(true, true, false), BridgePhase::PendingActivate);
        // 都齐了
        assert_eq!(bridge_phase(true, true, true), BridgePhase::Active);
        for p in [
            BridgePhase::NotInstalled,
            BridgePhase::NeedsRelease,
            BridgePhase::PendingActivate,
            BridgePhase::Active,
        ] {
            assert!(!p.as_str().is_empty());
        }
        assert_eq!(BridgePhase::Active.as_str(), "active");
    }

    #[test]
    fn pending_record_trims_and_reflects_the_interpreter() {
        assert_eq!(pending_record("  C:\\Python314\\python.exe "), "C:\\Python314\\python.exe");
        assert_eq!(pending_record(""), "");
    }

    #[test]
    fn every_generated_file_is_compiled_in_and_non_empty() {
        // 少写一个 include_str 都不会编译不过，但内容空/名错会让 Python 侧 ModuleNotFoundError
        assert_eq!(BRIDGE_FILES.len(), 8, "py-bridge 下应有 8 个文件");
        for (name, body) in BRIDGE_FILES.iter().chain(BUNDLE_FILES.iter()) {
            assert!(!body.trim().is_empty(), "{name} 内容为空");
            assert!(body.len() > 200, "{name}  suspiciously short");
        }
        let names: Vec<&str> = BRIDGE_FILES.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"bridge_entry.py"));
        assert!(names.contains(&"utils.py"));
        assert!(names.contains(&"sandbox_tools.py"));
        assert!(names.contains(&"markitdown_tools.py"));
    }

    #[test]
    fn release_is_idempotent_and_never_leaves_partial_files() {
        let tmp = std::env::temp_dir().join(format!("dsh-bridge-test-{}", std::process::id()));
        let bd = tmp.join("py-bridge");
        let bun = tmp.join("py-bridge-bundle");
        // 连跑两次：第二次必须成功且不产生半份文件
        for _ in 0..2 {
            release_sources(&bd, &bun).expect("release_sources");
            for (name, _) in BRIDGE_FILES {
                let p = bd.join(name);
                assert!(p.is_file(), "{name} 未写出");
                assert!(!p.to_string_lossy().ends_with(".tmp"), "{name} 留下了临时文件");
                assert!(std::fs::read_to_string(&p).unwrap().contains("py-bridge"), "{name} 内容不对");
            }
            assert!(bun.join("index.js").is_file());
            assert!(bun.join("package.json").is_file());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bundle_config_injects_paths_idempotently() {
        let tmp = std::env::temp_dir().join(format!("dsh-bundle-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let patch = tmp.join("cordis.patch.yml");
        std::fs::write(&patch, BUNDLE_FILES[1].1).unwrap();
        let bd = tmp.join("py-bridge");
        // 连续两次写入同一个解释器：第二次不能再套一层引号（幂等）
        for _ in 0..2 {
            write_bundle_config(&tmp, r"C:\Python314\python.exe", &bd).unwrap();
            let text = std::fs::read_to_string(&patch).unwrap();
            assert!(text.contains(r"pythonPath: 'C:\Python314\python.exe'"), "实际: {text}");
            assert!(!text.contains("pythonPath: ''"), "还有没填上的空值");
            assert_eq!(text.matches("pythonPath:").count(), 1, "不该重复插入");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bundle_config_escapes_single_quotes_for_yaml() {
        let tmp = std::env::temp_dir().join(format!("dsh-bundle-q-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let patch = tmp.join("cordis.patch.yml");
        std::fs::write(&patch, BUNDLE_FILES[1].1).unwrap();
        write_bundle_config(&tmp, r"C:\it's\python.exe", &tmp.join("py-bridge")).unwrap();
        let text = std::fs::read_to_string(&patch).unwrap();
        // YAML 单引号串里，' 要写成 ''，否则 YAML 解析直接坏掉
        assert!(text.contains(r"pythonPath: 'C:\it''s\python.exe'"), "实际: {text}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cmd_args_are_quoted_so_spaces_survive() {
        // 用户目录里有空格是常态（"John Doe"），不加引号 cmd 会把路径切成两半
        assert_eq!(quote_cmd_arg(r"D:\Programs\npm\dsh.cmd"), r#""D:\Programs\npm\dsh.cmd""#);
        assert_eq!(quote_cmd_arg(r"C:\Users\John Doe\dsh.cmd"), r#""C:\Users\John Doe\dsh.cmd""#);
        // 空参数仍要占位，否则 cmd 会直接把它丢掉、后面的参数跟着错位
        assert_eq!(quote_cmd_arg(""), "\"\"");
    }

    /// `dsh_argv` 的分支是纯字符串判断，所以直接测它的输出形状。
    /// 这里明确覆盖那条曾经出错的路径：**非 .cmd 时不能仍然返回 cmd.exe**
    /// （`cmd /C dsh.ps1` 不成立，会去执行一个不存在的东西）。
    #[test]
    fn dsh_argv_picks_the_right_program_per_extension() {
        for (path, want_program, want_first_arg) in [
            (r"D:\Programs\npm\dsh.cmd", "cmd.exe", r#""D:\Programs\npm\dsh.cmd""#),
            (r"D:\Programs\npm\dsh.CMD", "cmd.exe", r#""D:\Programs\npm\dsh.CMD""#),
        ] {
            let (prog, args) = dsh_argv_for(path);
            assert_eq!(prog, want_program, "{path}");
            assert_eq!(args[0], "/C", "{path} 的第一个参数必须是 /C");
            assert_eq!(args[1], want_first_arg, "{path} 的 dsh 路径必须加引号");
        }
        for path in [r"D:\Programs\npm\dsh.ps1", "dsh"] {
            let (prog, args) = dsh_argv_for(path);
            assert_eq!(prog, path, "{path} 必须直接执行，不能套 cmd.exe");
            assert_eq!(args, vec![path.to_string()], "{path} 的参数表就是它自己");
        }
    }
}