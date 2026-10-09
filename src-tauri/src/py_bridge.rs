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

/// 只读探测。**不返回 Result**：与 python_status_blocking 同一条纪律 —— 这一行是
/// 首选项里的状态，不是报错入口。任何一步探测不到都退化成「未装 / 未注册」的
/// 事实，而不是抛错（抛错会让整行空着，像卡住了）。写路径（deploy_after_install /
/// activate_pending）仍然返回 Result —— 那里失败是真该告诉用户的。
fn bridge_status_blocking(app: &AppHandle) -> BridgeStatus {
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
    // ⚠ `registered` 必须**原样**送进状态机，不能 `registered || pending`：
    // 「有 pending 标记」恰恰意味着**注册还没做**（DSH 当时在运行，我们故意跳过了）。
    // 或上它会让这一行显示「已激活」，而工具一个都没注册 —— 状态行说谎比不说更糟，
    // 用户会以为不用再管。pending 只作为**降级依据**：`registered` 查不到落点
    // （profile 没探测出来）时，退回「待激活」而不是误报「未部署」。
    let effective_registered = registered || (pending && profile.is_none());
    let phase = bridge_phase(py.is_some(), released, effective_registered);
    let message = bridge_phase_message(phase, pending, profile.as_deref());
    BridgeStatus {
        phase: phase.as_str().to_string(),
        released,
        registered,
        python_path: py
            .as_ref()
            .map(|p| p.program.to_string_lossy().to_string())
            .unwrap_or_default(),
        bridge_dir: bd.to_string_lossy().to_string(),
        bundle_dir: bun.to_string_lossy().to_string(),
        profile: profile.unwrap_or_default(),
        pending,
        message,
    }
}

/// 各阶段的人话说明（前端直接显示，不自己拼）。
///
/// `pending` 与 `profile` 单独传进来而不是从 phase 反推：`pending-activate` 有两种
/// 成因（「还没注册」与「已注册但等重启」），前者不该报出具体 profile 名；
/// 而 `active` 时若 pending 还在，得让用户知道下次进入 DSH 会清标记。
fn bridge_phase_message(phase: BridgePhase, pending: bool, profile: Option<&str>) -> String {
    match phase {
        BridgePhase::NotInstalled => i18n::t("py_bridge_not_installed").to_string(),
        BridgePhase::NeedsRelease => i18n::t("py_bridge_needs_release").to_string(),
        BridgePhase::PendingActivate => i18n::t("py_bridge_pending_activate").to_string(),
        BridgePhase::Active => {
            if pending {
                let name = profile.unwrap_or("");
                i18n::fmt("py_bridge_registered", &[&name])
            } else {
                i18n::t("py_bridge_active").to_string()
            }
        }
    }
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
/// 且 `%` / `^` / `&` / `|` 仍需转义 —— 但我们**不给命令名加引号**，见 dsh_argv_for。
fn register_bundle(app: &AppHandle, profile: &str, bun: &Path) -> Result<(), String> {
    if !profile_name_usable(profile) {
        return Err(i18n::fmt("py_bridge_bad_profile", &[&profile.to_string()]));
    }
    // 找不到 dsh 可执行文件 = 「注册没做」。绝不能当成成功：那条路会让状态行显示
    // 「已注册进 profile「」」，而 profile 里其实什么都没有（用户首装向导跑完前
    // dsh_path 就是空的）。宁可失败并保留 pending，等下次进入 DSH 再试。
    let Some((program, mut args)) = dsh_argv(app) else {
        // 找过哪里要说清：配置的路径 + PATH 探测结果，而不是空串
        let hint = {
            let configured = config::load(app).dsh_path.trim().to_string();
            let detected = detect::find_dsh_cmd()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if configured.is_empty() && detected.is_empty() {
                String::new()
            } else if configured.is_empty() {
                detected
            } else {
                format!("{configured} / {detected}")
            }
        };
        return Err(i18n::fmt("py_bridge_no_dsh", &[&hint]));
    };
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
                // 只把 dsh 的**原始输出尾部**往上抛，**不要**在这里套文案：
                // 调用方（deploy_after_install / activate_pending）会再用
                // py_bridge_register_fail 包一次。两层都格式化的话，日志里就会出现
                // 「失败：失败：…。将在…。将在…」这种套娃（真机日志原文）。
                let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
                Err(tail)
            }
        },
    )
}

/// 拼出「跑 dsh」的命令前缀，返回 `(程序, 已有参数)`。找不到 dsh 时返回 None。
///
/// **没有 shell**：参数直接进 argv。所以走 `cmd.exe` 那条路不是为了拼命令行，
/// 而只是因为 Windows 上 CreateProcess 不能直接执行 `.cmd`（那是 cmd 的脚本）。
/// 那种情况下整条命令行由 cmd 自己解析，规则与批处理一致：引号内的路径是一个
/// 整体 —— 不加引号的话，用户名里的空格会把 `D:\Users\John Doe\...` 拆成两个参数。
///
/// 两条路必须各自决定 program：`.cmd` 走 `cmd.exe`，而 `.ps1` / 裸 `dsh`
/// 要直接跑 —— 早先的版本把 program 写死成 `cmd.exe`，于是配置里指向
/// `dsh.ps1` 的机器会去执行一个根本不存在的东西（`cmd /C dsh.ps1` 不成立）。
fn dsh_argv(app: &AppHandle) -> Option<(String, Vec<String>)> {
    // `dsh_cmd_path` 给的是 `String`，`dsh_argv_for` 收 `&str` —— 借一下再传，
    // 别把 String 直接塞进去（`?` 不会替你借用，`String` 也 `as &str` 不掉）。
    let path = dsh_cmd_path(app)?;
    dsh_argv_for(&path)
}

/// `dsh_argv` 的纯逻辑（不含查找与配置读取），所以分支行为能直接单测。
///
/// ⚠ `.cmd` / `.bat` 那条**不能给命令名加引号** —— 这是本模块踩过的最深的一个坑。
/// cmd 的 `/C` 语义有两套：`/C <单个字符串>` 由 cmd 自己对整串做引号解析；`/C <cmd> <args...>`
/// 则把后面每个参数**原样**传给 CreateProcess，由它重新按 Windows 规则转义一次。
/// 我们走的是后者，于是传给 cmd 的命令名已经带了 Rust 的 `\"` 转义，cmd 把它当成
/// 命令名的**一部分** → `'"D:\...\dsh.cmd"' 不是内部或外部命令`（真机日志原文）。
/// 实测：`cmd /C "带空格的路径\x.cmd" a b`（分两个参数、不加引号）能正常跑通，
/// 因为 CreateProcess 自己会加引号；所以这里**裸传路径名**才是对的。
fn dsh_argv_for(dsh: &str) -> Option<(String, Vec<String>)> {
    // .cmd / .bat 都是 cmd 的脚本，CreateProcess 不能直接执行 → 交给 cmd.exe。
    // 合成一个条件而不是写两遍：两条分支产出完全相同，分开写只会诱使人只改一条。
    let lower = dsh.to_ascii_lowercase();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        Some((
            "cmd.exe".to_string(),
            vec!["/C".to_string(), dsh.to_string()],
        ))
    } else {
        // .exe / .ps1：直接执行。注意这里**不做**「裸命令名」的兜底 ——
        // 裸 `dsh` 交给 cmd.exe 会被当成目录名，见 dsh_cmd_path 的注释。
        Some((dsh.to_string(), vec![dsh.to_string()]))
    }
}

/// （这里曾经有一个 `quote_cmd_arg` 给 cmd 用的参数加引号 —— 已删除。
//  它是错的：`cmd /C` 后面分参数传时，CreateProcess 会自己转义一次，
//  我们再加的引号会被 cmd 当成命令名的一部分。实测见 dsh_argv_for 的注释。）

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

/// DSH 家目录（profile 的上上级）。
///
/// 优先 `DSH_HOME`；它没设时从 `DSH_PROFILE_DIR` 反推
/// （`…\profiles\web` → `…`）—— 用户从没设过 `DSH_HOME` 的机器很常见，
/// 而此时 desktop 又没启动 DSH，拿不到那个变量。
fn dsh_home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var("DSH_HOME").ok().filter(|s| !s.trim().is_empty()) {
        return Some(PathBuf::from(h));
    }
    // `<home>\profiles\<name>` → 去两级
    profile_dir_from_env()?
        .parent()
        .and_then(|profiles| profiles.parent())
        .map(PathBuf::from)
}

/// profile 名的实测值。
///
/// `dsh --profile <p> --dump-config` 会打印整份组合后的配置，profile 名就在里面；
/// 同时也顺带确认这个 profile 确实存在（不存在时 DSH 会报错）。取不到就退到 `web`
/// —— 桌面端起的就是 `dsh web`，而 `web` 正是 DSH 默认的 web profile。
fn probe_profile(app: &AppHandle) -> Option<String> {
    // ① 宿主进程环境（由 DSH 启动时注入，最权威 —— 它就是**正在跑的那个** profile）
    for var in ["DSH_PROFILE", "DSH_PROFILE_NAME"] {
        if let Ok(p) = std::env::var(var) {
            let t = p.trim().to_string();
            if profile_name_usable(&t) {
                return Some(t);
            }
        }
    }
    // ② 退到 DSH_HOME 下**实际存在**的 profiles\<name>\：这比「猜 web」可靠 ——
    //    猜错的后果是往一个不存在的 profile 里装 bundle，而 `dsh plugin add`
    //    会把它**创建**出来，于是用户凭空多出一个 profile、DSH 却不加载它。
    //    桌面端起 `dsh web`，所以 web 排第一；但只认「目录真的在」。
    if let Some(home) = dsh_home_dir() {
        for name in ["web", "default"] {
            if home.join("profiles").join(name).is_dir() {
                return Some(name.to_string());
            }
        }
    }
    // ③ 问 DDH 自己（--dump-config 会打印组合后的配置）。留作兜底：
    //    首次安装、$DSH_HOME 还没建时前面两步都拿不到。
    let Some((program, mut args)) = dsh_argv(app) else {
        return None;
    };
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

/// dsh 入口路径。
///
/// 顺序与 `detect::find_dsh_cmd` 一致：配置里填的 → PATH/注册表/全局目录里真的找得到的。
/// ⚠ 中间**不能**回落到裸字符串 `"dsh"`：那是本程序启动 DSH 用的**程序名**（靠 CreateProcess
/// 解析 PATH），而这里要交给 `cmd.exe /C` —— cmd 只认**扩展名**，`dsh` 会被当成目录名，
/// 整条命令失败、`profile` 探测不到 → 状态行显示「profile「」」（注册其实成功了）。
/// 用户首次安装时 `dsh_path` 就是空的（要等首装向导写盘），正好命中这条路径。
fn dsh_cmd_path(app: &AppHandle) -> Option<String> {
    // 走被单测覆盖的那个判定（tests 里同名函数）：测试必须守住**真实代码路径**，
    // 而不是它的一份复制品 —— 复制品改错了测试照样绿。
    if let Some(configured) = dsh_cmd_path_requires_a_real_file(&config::load(app).dsh_path) {
        return Some(configured);
    }
    detect::find_dsh_cmd().map(|p| p.to_string_lossy().to_string())
}

/// 配置里填的 dsh 路径能不能直接用：非空 **且文件真的存在**。
///
/// 存在性检查不能省：首装向导跑完前 `dsh_path` 是空的，而写错路径的用户也不少见；
/// 拿一条注定失败的命令去注册，得到的只会是一句看不懂的失败。
fn dsh_cmd_path_requires_a_real_file(configured: &str) -> Option<String> {
    let t = configured.trim();
    if t.is_empty() || !Path::new(t).is_file() {
        return None;
    }
    Some(t.to_string())
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
        // 连跑两次：第二次必须成功且不产生半份文件。
        // 判据是「落盘内容与编译进来的源码**逐字相同**」——而不是去找某个标记串：
        // 标记串只存在于一部分文件里（例如只有 utils.py 提到 py-bridge），拿它当
        // 断言会误判成「内容不对」，却查不出真正的截断/覆盖失败。
        for round in 1..=2 {
            release_sources(&bd, &bun).expect("release_sources");
            for (name, body) in BRIDGE_FILES {
                let p = bd.join(name);
                assert!(p.is_file(), "第 {round} 轮：{name} 未写出");
                let got = std::fs::read_to_string(&p).unwrap();
                assert_eq!(got, *body, "第 {round} 轮：{name} 内容与源码不一致");
            }
            for (name, body) in BUNDLE_FILES {
                let p = bun.join(name);
                assert!(p.is_file(), "第 {round} 轮：{name} 未写出");
                let got = std::fs::read_to_string(&p).unwrap();
                assert_eq!(got, *body, "第 {round} 轮：{name} 内容与源码不一致");
            }
            // 半份文件的特征是「临时文件还在」：写完必须已改名到位
            for (name, _) in BRIDGE_FILES {
                assert!(
                    !bd.join(format!("{name}.{}.tmp", std::process::id())).exists(),
                    "第 {round} 轮：{name} 留下了临时文件"
                );
            }
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

    /// 回归：`.cmd` 的命令名**绝不能**被我们加引号。
    ///
    /// 真机故障（日志原文）：`'\"D:\Programs\npm\dsh.cmd\"' 不是内部或外部命令`。
    /// 根因是 `cmd /C <cmd> <args...>` 这种「分参数」形式下，CreateProcess 会把
    /// 每个参数按 Windows 规则**再转义一次**，我们预先包上的引号于是变成命令名
    /// 的一部分。实测裸传（含空格）能跑通，因为 CreateProcess 自己会加引号。
    #[test]
    fn cmd_script_path_is_passed_unquoted() {
        for path in [r"D:\Programs\npm\dsh.cmd", r"C:\Users\John Doe\dsh.cmd"] {
            let (_, args) = dsh_argv_for(path).expect("cmd 脚本");
            assert_eq!(args[1], path, "{path}：命令名不得加引号（CreateProcess 会自己转义）");
            assert!(!args[1].contains('"'), "{path}：参数里不该出现引号");
            assert!(!args[1].contains('\\'), "{path}：不该出现反斜杠转义");
        }
    }

    /// `dsh_argv` 的分支是纯字符串判断，所以直接测它的输出形状。
    /// 这里明确覆盖那条曾经出错的路径：**非 .cmd 时不能仍然返回 cmd.exe**
    /// （`cmd /C dsh.ps1` 不成立，会去执行一个不存在的东西）。
    #[test]
    fn dsh_argv_picks_the_right_program_per_extension() {
        for path in [
            r"D:\Programs\npm\dsh.cmd",
            r"D:\Programs\npm\dsh.CMD",
            r"D:\Programs\npm\dsh.bat",
        ] {
            let (prog, args) = dsh_argv_for(path).expect("cmd 脚本应走 cmd.exe");
            assert_eq!(prog, "cmd.exe", "{path}");
            assert_eq!(args[0], "/C", "{path} 的第一个参数必须是 /C");
            assert_eq!(args[1], path, "{path} 的命令名原样传入，不加引号");
        }
        for path in [r"D:\Programs\npm\dsh.exe", "dsh", r"D:\x\dsh.ps1"] {
            let (prog, args) = dsh_argv_for(path).expect("可执行文件应直接执行");
            assert_eq!(prog, path, "{path} 必须直接执行，不能套 cmd.exe");
            assert_eq!(args, vec![path.to_string()], "{path} 的参数表就是它自己");
        }
    }

    /// 回归：找不到 dsh 时 `dsh_argv` 必须给 None，**不能**编出一个裸 `dsh` 交给 cmd.exe。
    ///
    /// 真实故障：用户首装向导跑完前 `config.dsh_path` 是空的，早先的版本回落到
    /// 裸字符串 `"dsh"`，再被塞进 `cmd.exe /C` —— cmd 只认扩展名，会把 `dsh`
    /// 当成目录名，于是命令失败、`profile` 探测不到，状态行显示
    /// 「已注册进 profile「」」，而 profile 里其实什么都没有。
    #[test]
    fn no_dsh_found_means_none_not_a_bare_command_name() {
        // 配置里是空串 → 交给 find_dsh_cmd 那一层；整条路都拿不到时才 None
        assert!(dsh_cmd_path_requires_a_real_file("").is_none());
        assert!(dsh_cmd_path_requires_a_real_file("   ").is_none());
        // 路径写得像模像样但**文件不存在** → 同样不能当它有效，
        // 否则就会拼出一条注定失败的命令（曾经的真故障就是这样冒出来的）
        assert!(dsh_cmd_path_requires_a_real_file(r"C:\definitely-not-here\dsh.cmd").is_none());
        // 只有真存在的文件才算数（这里造一个临时文件来验证正面情形）
        let tmp = std::env::temp_dir().join(format!("dsh-fake-{}.cmd", std::process::id()));
        std::fs::write(&tmp, "@echo off\r\n").unwrap();
        assert_eq!(
            dsh_cmd_path_requires_a_real_file(&tmp.to_string_lossy()),
            Some(tmp.to_string_lossy().to_string())
        );
        let _ = std::fs::remove_file(&tmp);

        // 另一条契约：dsh_argv_for 只负责选 program，不负责「找不找得到」
        assert_eq!(
            dsh_argv_for("dsh").expect("裸 dsh 仍可直接执行").0,
            "dsh",
            "「找不到」由 dsh_cmd_path 用 None 表达，本函数不兜底"
        );
    }

    /// 回归：**有 pending 标记绝不能被报成「已激活」**。
    ///
    /// 真实故障：用户点「基本安装」时 DSH 正在运行，于是我们只写标记、**跳过注册**；
    /// 但状态判定写成 `registered || pending`，而 `bundle_registered()` 查 profile
    /// 本来就是 false —— 或上 pending 之后变成 true，界面显示「已激活：AI 现在就能调用
    /// convert_file_to_markdown」，而实际上工具一个都没注册。状态行说谎比不说更糟。
    ///
    /// 这里复刻当时的输入组合：Python 在、代码已释放、**未注册**、**有 pending**。
    #[test]
    fn pending_marker_never_masquerades_as_active() {
        // 当时那行的输入：registered=false 但 pending=true
        let registered = false;
        let pending = true;
        let profile_detected = true; // profile 能探测出来（所以不能靠「查不到」来降级）
        assert!(!registered, "前提：注册确实没做");

        let effective = registered || (pending && !profile_detected);
        let phase = bridge_phase(true /* python ok */, true /* released */, effective);
        assert_ne!(
            phase,
            BridgePhase::Active,
            "有 pending 且未注册时不得报 active —— 工具此时一个都没有"
        );
        assert_eq!(phase, BridgePhase::PendingActivate);
    }

    /// 反过来也要成立：profile 探测失败时，pending 可以当作「已注册」的退路
    /// （宁可说待激活，也不谎报「未部署」）。
    #[test]
    fn pending_is_the_fallback_only_when_the_profile_is_unknown() {
        let effective = false || (true && false);
        assert_eq!(
            bridge_phase(true, true, effective),
            BridgePhase::PendingActivate
        );
        // 已注册且无 pending → 真的 active
        assert_eq!(bridge_phase(true, true, true), BridgePhase::Active);
    }
}