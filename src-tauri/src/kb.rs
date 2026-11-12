// DSH-Desktop —— 「记忆，技能与知识库」安装块：dsh-kb（知识层）+ dsh-memory2skill（记忆/技能层）
//
// 模板是 `office.rs`（装/卸一个 bundle + 后端合成一行状态），**不是** `py_bridge.rs`
// —— 后者刻意只有只读的 `bridge_status`、没有安装命令（见 lib.rs 里那段注释）。
//
// 与 office 的两处刻意差异：
//   ① 这里嵌的是**成品 tgz**（`include_bytes!`），安装时不再 `npm pack`：
//      少一个 npm 依赖，也省掉几十秒打包。代价是版本钉死在构建期 —— 升级插件
//      要换文件 + 改 `include_bytes!` 路径 + 重新构建 Desktop（与 office 嵌源码
//      同一性质，可接受）。包名不变，下面的 `KB_PKG` / `MEMORY_PKG` 不用跟着改。
//   ② 一次动作装**两个**包，所以 `kb_pending` 的值是「还差注册的包名」逗号列表：
//      部分失败只把失败的那个留在列表里，下次进入 DSH 时自愈重试那一个。
//
// 这个块的增量价值只有三件（DSH 侧边栏「插件」页与 plugin_manager 已经能做
// 全部单包操作，还带预检 / 取消 / 重试 / 脚本审批）：一次装两个包、tgz 随
// Desktop 分发（用户不用去找路径）、一行用「记忆/技能/知识库」人话写的合并状态。
// 所以这里**不**重实现 inspect 失败分类、安装源探测、脚本审批 —— 那些用宿主的。
//
// 状态判定的可行性边界（已核实，方案 8.6 的原设想做不到 CLI 化）：
//   · `dsh plugin` 是 pnpm 转发器（`add` / `remove` / `why`），没有报告 DSH 解析
//     状态的 `list`；`listBundles` 是**运行中宿主**的运行时服务，Desktop 在 DSH
//     进程外拿不到。
//   · 于是照 office 的既有模型判状态：`bundle_registered_named`（查
//     `<profile>/node_modules/<pkg>/package.json` 是否存在）+ pending 标记。
//   · **诚实纪律**（照 office.rs:370-378）：「已写进 profile」≠「DSH 已激活工具」。
//     文案只说「已注册进 profile，进入 DSH 后生效」，绝不把「文件系统里有」写成
//     「工具已可用」。真要确认激活，得在 DSH 在跑时 HTTP 查宿主插件清单看 bundle
//     有没有带 `error` —— 那是可选增强，本轮没做。
//   · 注册用 `file:<tgz>`（不是 `link:`）。理由**不是**「link: 解析不到」（那条
//     过时了：现行 plugin-manager 支持 link: 作开发态源），真实理由是
//     ① link: 指向开发机目录，用户机上不存在；② link: 用源 checkout 自己的
//     node_modules 解析依赖，踩依赖投影坑。发布态必须自带 tgz。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::i18n;
use crate::logger;
use crate::process::{self, AppState, SetupResult};

// ---------- 模块级常量 ----------

/// 知识层（主功能）与记忆/技能层（增强）在 profile 里的包名。
/// 都是**不带 scope 的裸名**：`bundle_registered_named` 按 `/` 拆路径段，
/// 裸名同样成立（落点 `<profile>/node_modules/dsh-kb/package.json`）。
const KB_PKG: &str = "dsh-kb";
const MEMORY_PKG: &str = "dsh-memory2skill";

/// 两个成品 tgz 的文件名（带版本号；升级 = 换 resources 里的文件 + 改这两行
/// + 改下面 include_bytes! 的路径，三处必须一起改，有单测守住）
const KB_TGZ: &str = "dsh-kb-0.4.2.tgz";
const MEMORY_TGZ: &str = "dsh-memory2skill-0.4.9.tgz";

/// 释放进 `<应用配置目录>\kb-bundle\` 的两个包（与 `resources/kb-bundle/` 一一对应）。
/// 少一条就等于那个插件永远装不上，构建期就会报路径错。
const KB_BUNDLE_FILES: &[(&str, &[u8])] = &[
    (KB_TGZ, include_bytes!("../resources/kb-bundle/dsh-kb-0.4.2.tgz")),
    (
        MEMORY_TGZ,
        include_bytes!("../resources/kb-bundle/dsh-memory2skill-0.4.9.tgz"),
    ),
];

/// 安装/注册的**顺序**：知识层在前（主功能），记忆层在后（增强）。
/// 状态文案的主次、以及「先保住能用的那半边」都跟着这个顺序走。
const KB_PACKAGES: [(&str, &str); 2] = [(KB_PKG, KB_TGZ), (MEMORY_PKG, MEMORY_TGZ)];

/// 释放目录名（`<应用配置目录>\kb-bundle`）
const BUNDLE_DIR_NAME: &str = "kb-bundle";
/// config.json 里的 pending 键。`kb_pending` **同时**是 Config 正式字段
/// （见 config.rs 那个字段的三条真机教训：正式字段 + 单键读写 + 自愈进门，缺一不可）
const PENDING_KEY: &str = "kb_pending";

/// 一次 `dsh plugin add/remove` 的超时：那是一次 pnpm 安装，实测几十秒到几分钟。
/// 与 office / py_bridge 同一档。
const PLUGIN_CMD_TIMEOUT_SECS: u64 = 10 * 60;

/// 写一行 [kb] 日志（desktop.log 里一眼认得出是哪条链）
fn log_line(app: &AppHandle, line: &str) {
    logger::append_line(&logger::desktop_log_path(app), &format!("[kb] {line}"));
}

// ---------- 释放目录 ----------

/// `<应用配置目录>\kb-bundle`
pub fn bundle_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(BUNDLE_DIR_NAME)
}

/// 两个 tgz 都在才算「已释放」（空壳目录、半份包都不算 —— 与 office 同一口径）
fn bundle_released(bun: &Path) -> bool {
    KB_BUNDLE_FILES.iter().all(|(name, _)| bun.join(name).is_file())
}

/// 幂等释放：临时文件 + 原子改名覆盖（照 office::write_files 的字节版）。
/// 重复点安装不会留下半份文件；rename 失败先删临时文件再报错。
fn write_binary_files(dir: &Path, files: &[(&str, &[u8])]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        i18n::fmt("kb_write_fail", &[&dir.to_string_lossy(), &e.to_string()])
    })?;
    for (name, body) in files {
        let target = dir.join(name);
        // 临时名带进程号：同名文件不会被自己覆盖成空
        let tmp = dir.join(format!("{}.{}.tmp", name, std::process::id()));
        std::fs::write(&tmp, body).map_err(|e| {
            i18n::fmt("kb_write_fail", &[&tmp.to_string_lossy(), &e.to_string()])
        })?;
        std::fs::rename(&tmp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            i18n::fmt("kb_write_fail", &[&target.to_string_lossy(), &e.to_string()])
        })?;
    }
    Ok(())
}

/// 幂等释放两个 tgz
fn release_bundles(bun: &Path) -> Result<(), String> {
    write_binary_files(bun, KB_BUNDLE_FILES)
}

/// 删释放目录；「被占用」（os error 32/5，比如某次 `dsh plugin add` 还在读它）
/// 单独归类 —— 里面的文件已删光时，空壳会在下次进入 DSH 时由
/// [`cleanup_stale_kb_dirs`] 清掉（照 office::remove_bundle_dir:142-151）。
fn remove_bundle_dir(dir: &Path) -> Result<(), (String, bool)> {
    if !dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(dir).map_err(|e| {
        let locked = matches!(e.raw_os_error(), Some(32) | Some(5))
            || e.kind() == std::io::ErrorKind::PermissionDenied;
        (e.to_string(), locked)
    })
}

/// 目录整体删不掉时（被某次还在跑的 `dsh plugin add` 占住），把**里面的 tgz 逐个删**：
/// `bundle_released` 立刻变 false，进门自愈就不再把它重新装回去（否则「卸载」会在下次
/// 进入 DSH 时被自愈链无声撤销），剩下的空壳交给 [`cleanup_stale_kb_dirs`] 下次清掉。
/// 返回删掉的个数。
fn remove_released_files(bun: &Path) -> usize {
    let mut gone = 0usize;
    for (name, _) in KB_BUNDLE_FILES.iter() {
        let p = bun.join(name);
        if p.is_file() && std::fs::remove_file(&p).is_ok() {
            gone += 1;
        }
    }
    gone
}

/// 进门清理空壳目录（DSH 刚起来、没有 `dsh plugin add` 在跑，目录不再被锁）。
/// 返回 Some = 清掉了，值得一行日志。
pub fn cleanup_stale_kb_dirs(app: &AppHandle) -> Option<String> {
    let dir = bundle_dir(&config::config_dir(app));
    if !dir.exists() {
        return None;
    }
    // 非空 → 是有效的安装包，不动
    match std::fs::read_dir(&dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return None;
            }
        }
        Err(_) => return None,
    }
    if std::fs::remove_dir(&dir).is_ok() {
        let line = i18n::fmt("kb_stale_dir_removed", &[&dir.to_string_lossy()]);
        log_line(app, &line);
        return Some(line);
    }
    None
}

// ---------- phase 状态机（纯函数，必须可单测） ----------

/// 两个包共用同一套四态：由「tgz 释放了吗 / 这个包进 profile 了吗」推出界面该显示什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbPhase {
    /// 未安装（没释放、profile 里也没有）
    NotInstalled,
    /// profile 里有注册残留、但释放目录被清掉了（不影响已注册的工具，activate_pending 会补释放）
    NeedsRelease,
    /// tgz 已释放、还没写进 profile（首次安装 / DSH 在跑时留下的 pending）
    PendingActivate,
    /// 已在 profile 里。⚠ 这只是**文件系统事实**，不等于 DSH 已加载工具。
    Active,
}

impl KbPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NeedsRelease => "needs-release",
            Self::PendingActivate => "pending-activate",
            Self::Active => "active",
        }
    }
}

/// 纯函数：状态机全部输入就是这两个布尔（照 `office_bridge_phase:208-215`）。
/// pending 标记不参与判定 —— 它意味着「注册还欠着」，只影响文案与降级
/// （profile 没探测出来时退回「待激活」而不是误报「未安装」）。
pub fn kb_phase(released: bool, registered: bool) -> KbPhase {
    match (released, registered) {
        (false, false) => KbPhase::NotInstalled,
        (false, true) => KbPhase::NeedsRelease,
        (true, false) => KbPhase::PendingActivate,
        (true, true) => KbPhase::Active,
    }
}

/// 单个包的短语（其余组合时逐包点名用）。
///
/// Active 分支的诚实口径（照 office.rs:370-378）：pending 还挂着这个包时，
/// 说的是「已注册进 profile，进入 DSH 后生效」，**不谎称已激活**。
fn package_phrase(phase: KbPhase, pending_here: bool, profile: &str) -> String {
    match phase {
        KbPhase::NotInstalled => i18n::t("kb_not_installed").to_string(),
        KbPhase::NeedsRelease => i18n::t("kb_needs_release").to_string(),
        KbPhase::PendingActivate => i18n::t("kb_pending_activate").to_string(),
        KbPhase::Active => {
            // ⚠ `i18n::fmt` 收的是 `&[&dyn Display]`：直接递 `&str` 会因 `str` 不定长
            // 报 E0277（office.rs 里同样的坑），这里多包一层引用。
            if pending_here {
                i18n::fmt("kb_registered_pending", &[&profile])
            } else {
                i18n::fmt("kb_active", &[&profile])
            }
        }
    }
}

/// 合并状态文案（纯函数，可单测）：两个包的 phase + 两个 pending 位 → 一句人话。
///
/// 四种主组合照方案 8.6；**其余组合一律逐包点名**，这样「只装成一个」永远不会
/// 被笼统的「已就绪」盖掉（方案 8.9 第 5 条，这套设计的三根柱子之一）。
pub fn kb_merged_message(
    kb: KbPhase,
    kb_pending: bool,
    memory: KbPhase,
    memory_pending: bool,
    profile: Option<&str>,
) -> String {
    // prof 转成 String：`i18n::fmt` 的参数槽是 `&dyn Display`，递裸 `&str` 会撞上
    // 「str 没有大小」的 E0277（office.rs 同款坑，那里靠多包一层引用解决）
    let prof = profile.unwrap_or("").to_string();
    // 「这一层已经算装好了」= 在 profile 里，且不再欠着注册
    let kb_ready = kb == KbPhase::Active && !kb_pending;
    let memory_ready = memory == KbPhase::Active && !memory_pending;
    // 两个都已释放、都还没注册：DSH 在跑时点安装的典型形状（等下次进入生效）
    let both_deferred = kb == KbPhase::PendingActivate && memory == KbPhase::PendingActivate;

    if kb_ready && memory_ready {
        i18n::fmt("kb_msg_both_ready", &[&prof])
    } else if both_deferred {
        i18n::fmt("kb_msg_pending_activate", &[&prof])
    } else if kb_ready && memory == KbPhase::NotInstalled {
        i18n::fmt("kb_msg_kb_only", &[&prof])
    } else if memory_ready && kb == KbPhase::NotInstalled {
        i18n::fmt("kb_msg_memory_only", &[&prof])
    } else if kb == KbPhase::NotInstalled
        && memory == KbPhase::NotInstalled
        && !kb_pending
        && !memory_pending
    {
        i18n::t("kb_msg_none").to_string()
    } else {
        // 部分失败 / 异常组合：**按包名各说一句**。这里刻意不写成一句好话 ——
        // 「只装成一个」如果被笼统的「已就绪」盖掉，用户就会以为记忆工具该出现却没有
        // （方案 8.9 第 5 条，三根柱子之一），所以宁可说得技术一点也要点得出名。
        i18n::fmt(
            "kb_msg_each",
            &[
                &KB_PKG,
                &package_phrase(kb, kb_pending, &prof),
                &MEMORY_PKG,
                &package_phrase(memory, memory_pending, &prof),
            ],
        )
    }
}

// ---------- 状态（kb_status 命令：只读，绝无副作用） ----------

/// 「记忆，技能与知识库」块的状态。`message` 是后端按当前语言拼好的合并人话，
/// 前端只显示、不拼句子（判定条件只有一处定义）。
#[derive(Clone, Serialize)]
pub struct KbStatus {
    /// 知识层 dsh-kb 的 phase（KbPhase 的字符串形态）
    pub kb_phase: String,
    /// 记忆/技能层 dsh-memory2skill 的 phase
    pub memory_phase: String,
    /// 实际使用的 profile（没探测到为空串）
    pub profile: String,
    /// 合并状态行
    pub message: String,
}

#[tauri::command]
pub async fn kb_status(app: AppHandle) -> Result<KbStatus, String> {
    // 探测里可能起子进程（probe_profile 的 `--dump-config` 兜底那一步），
    // 下到阻塞线程池（照 office_status:257-263 的纪律，别占运行时的工作线程）
    tauri::async_runtime::spawn_blocking(move || kb_status_blocking(&app))
        .await
        .map_err(|e| e.to_string())
}

fn kb_status_blocking(app: &AppHandle) -> KbStatus {
    // 任何一步探测失败都退化成「未安装」，不报错：状态行不是报错入口（照 office）
    let bun = bundle_dir(&config::config_dir(app));
    let released = bundle_released(&bun);
    let profile = crate::py_bridge::probe_profile(app);
    let registered_of = |pkg: &str| -> bool {
        profile
            .as_deref()
            .map(|p| crate::py_bridge::bundle_registered_named(p, pkg))
            .unwrap_or(false)
    };
    let kb_registered = registered_of(KB_PKG);
    let memory_registered = registered_of(MEMORY_PKG);
    let pending = pending_list(app);
    let kb_pending = pending.iter().any(|p| p == KB_PKG);
    let memory_pending = pending.iter().any(|p| p == MEMORY_PKG);

    // pending 只作降级依据：profile 探测不出来时（用户没设 DSH_HOME、也没在跑 DSH），
    // 「已写入 pending」比「未安装」更贴近事实 —— 与 office_status:293-295 同一条纪律
    let no_profile = profile.is_none();
    let kb = kb_phase(released || (kb_pending && no_profile), kb_registered);
    let memory = kb_phase(released || (memory_pending && no_profile), memory_registered);

    KbStatus {
        kb_phase: kb.as_str().to_string(),
        memory_phase: memory.as_str().to_string(),
        profile: profile.clone().unwrap_or_default(),
        message: kb_merged_message(kb, kb_pending, memory, memory_pending, profile.as_deref()),
    }
}

// ---------- pending 标记（单键读写；值是「还差注册的包名」逗号列表） ----------

/// 读 pending 列表。空串 / 缺键 / 全空白 → 空列表（清除标记写的是空串，不是删键）。
fn pending_list(app: &AppHandle) -> Vec<String> {
    config::load(app)
        .kb_pending
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 写 pending 列表（`write_config_key` 单键读-改-写，绝不整份覆盖别的设置）；
/// 空列表 = 清除标记。
fn write_pending_list(app: &AppHandle, list: &[String]) -> Result<(), String> {
    config::write_config_key(app, PENDING_KEY, &list.join(","))
}

/// 把「这次已经装好的包」从 pending 里去掉，失败的那个留着等下次自愈重试。
/// 写失败**不**往上抛：后果只是「下次多注册一次」（幂等），而抛错会把
/// 一个已经注册成功的包报成失败，那才是真危害。
fn drop_pending(app: &AppHandle, pkg: &str) {
    let kept: Vec<String> = pending_list(app).into_iter().filter(|p| p != pkg).collect();
    if let Err(e) = write_pending_list(app, &kept) {
        log_line(app, &format!("清除 {pkg} 的 pending 失败：{e}"));
    }
}

fn clear_pending(app: &AppHandle) {
    if let Err(e) = write_pending_list(app, &[]) {
        log_line(app, &format!("清除 pending 失败：{e}"));
    }
}

// ---------- 注册 / 摘除（照 office::register_office_bundle / remove_office_bundle） ----------

/// `dsh plugin --profile <p> add file:<tgz>`。
///
/// ⚠ 与 office 的关键差别：**这里不跑 `npm pack`** —— 嵌的已经是成品 tgz，直接交给
/// CLI（省掉安装路径对 npm 的依赖与几十秒打包）。
/// spec 必须是 `file:` + tgz 绝对路径，反斜杠转正斜杠（office.rs:840 同款写法）。
///
/// 失败时只回原始输出尾部，不在这里套文案：调用方还会包一层，两层都格式化会在
/// 日志里出现「失败：失败：…」套娃（py_bridge 的真机日志原文）。
fn register_kb_package(app: &AppHandle, profile: &str, tgz: &Path) -> Result<(), String> {
    if !crate::py_bridge::profile_name_usable(profile) {
        return Err(i18n::fmt("kb_bad_profile", &[&profile]));
    }
    let Some((program, mut args)) = crate::py_bridge::dsh_argv(app) else {
        let hint = {
            let configured = config::load(app).dsh_path.trim().to_string();
            let detected = crate::detect::find_dsh_cmd()
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
        return Err(i18n::fmt("kb_no_dsh", &[&hint]));
    };
    if !tgz.is_file() {
        return Err(i18n::fmt("kb_missing_tgz", &[&tgz.to_string_lossy()]));
    }
    let file_spec = format!("file:{}", tgz.to_string_lossy().replace('\\', "/"));
    args.extend([
        "plugin".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "add".to_string(),
        file_spec,
    ]);
    let (ok, out) = process::run_cmd_capture(
        &program,
        &args,
        "",
        Duration::from_secs(PLUGIN_CMD_TIMEOUT_SECS),
    )?;
    if ok {
        Ok(())
    } else {
        let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
        Err(tail)
    }
}

/// `dsh plugin --profile <p> remove <pkg>`。
/// **必须走 CLI**：包同时占 profile 的 package.json dependencies 与
/// dsh.profile.bundles 两处，手删 node_modules 会留下幽灵清单条目（照 office）。
fn remove_kb_package(app: &AppHandle, profile: &str, pkg: &str) -> Result<(), String> {
    if !crate::py_bridge::profile_name_usable(profile) {
        return Err(i18n::fmt("kb_bad_profile", &[&profile]));
    }
    let Some((program, mut args)) = crate::py_bridge::dsh_argv(app) else {
        return Err(i18n::fmt("kb_no_dsh", &[&String::new()]));
    };
    args.extend([
        "plugin".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "remove".to_string(),
        pkg.to_string(),
    ]);
    let (ok, out) = process::run_cmd_capture(
        &program,
        &args,
        "",
        Duration::from_secs(PLUGIN_CMD_TIMEOUT_SECS),
    )?;
    if ok {
        Ok(())
    } else {
        let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
        Err(tail)
    }
}

/// profile 探测 + 兜底（照 office::deploy_office_after_install:1130-1136）：
/// 探不到就退 `web`（桌面端起的就是 `dsh web`），并留一行日志说明这是兜底值。
fn resolve_profile(app: &AppHandle) -> String {
    crate::py_bridge::probe_profile(app).unwrap_or_else(|| {
        log_line(app, "probe_profile 失败，退回 profile \"web\"");
        "web".to_string()
    })
}

/// 注册一个包，并把结果同步进 pending（成功去掉那一项，失败把那一项留着）。
/// 返回的 Err 里带**包名**与原始输出尾部 —— 部分失败必须点得出名。
fn register_with_pending(app: &AppHandle, profile: &str, pkg: &'static str, bun: &Path) -> Result<(), String> {
    let tgz = KB_PACKAGES
        .iter()
        .find(|(name, _)| *name == pkg)
        .map(|(_, file)| bun.join(file));
    let outcome = match tgz {
        Some(path) => register_kb_package(app, profile, &path),
        None => Err(i18n::fmt("kb_unknown_pkg", &[&pkg])),
    };
    match outcome {
        Ok(()) => {
            drop_pending(app, pkg);
            let line = i18n::fmt("kb_registered", &[&pkg, &profile]);
            process::setup_progress(app, "deploy", &line);
            log_line(app, &line);
            Ok(())
        }
        Err(e) => {
            // 失败：把这个包留在 pending 里，下次进入 DSH 由 activate_pending 重试
            let mut list = pending_list(app);
            if !list.iter().any(|p| p == pkg) {
                list.push(pkg.to_string());
                if let Err(ce) = write_pending_list(app, &list) {
                    log_line(app, &format!("写入 pending 失败：{ce}"));
                }
            }
            let line = i18n::fmt("kb_register_fail", &[&pkg, &e]);
            process::setup_progress(app, "deploy", &line);
            log_line(app, &line);
            Err(line)
        }
    }
}

// ---------- 任务框架（照 office::spawn_office_task:892-946） ----------

/// 抢 `setup_busy` → 开线程跑任务 → 统一回传 `setup-result`（target="kb"）→
/// **无论如何释放 busy**。与 Python / OnlyOffice 共用同一把锁：同一时刻只允许
/// 一个引导级任务，两条进度流搅在一起没有意义。
fn spawn_kb_task(
    app: AppHandle,
    target: &'static str,
    work: impl FnOnce(&AppHandle) -> Result<String, String> + Send + 'static,
) -> Result<(), String> {
    if app.state::<AppState>().setup_busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(i18n::t("err_setup_busy").to_string());
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        // panic 也必须走完收尾：work 一旦 unwind，emit 与 busy 复位都不会执行，
        // 前端按钮会**永久**停在「安装中」。GUI 程序没有 stderr，原因必须落盘。
        let mut panic_detail: Option<String> = None;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&app2)))
            .unwrap_or_else(|payload| {
                panic_detail = Some(
                    payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic payload".to_string()),
                );
                Err(i18n::t("setup_kb_panicked").to_string())
            });
        let (ok, msg) = match outcome {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        // 收尾本身也兜一层：记日志/发事件再 panic 时，busy 复位仍然要执行
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(detail) = &panic_detail {
                log_line(&app2, &format!("kb 任务异常终止（panic）：{detail}"));
            }
            log_line(
                &app2,
                &i18n::fmt(
                    "setup_kb_result_line",
                    &[
                        &i18n::t(if ok { "setup_word_ok" } else { "setup_word_fail" }),
                        &msg,
                    ],
                ),
            );
            let _ = app2.emit(
                "setup-result",
                SetupResult { target: target.to_string(), success: ok, message: msg },
            );
        }));
        app2.state::<AppState>().setup_busy.store(false, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(())
}

// ---------- 安装 ----------

#[tauri::command]
pub async fn kb_install(app: AppHandle) -> Result<(), String> {
    spawn_kb_task(app, "kb", kb_install_blocking)
}

/// 顺序装两个包：tgz 释放 → 定 profile → DSH 在跑就留 pending → 否则就地注册。
///
/// 竞态纪律（照 office::deploy_office_after_install:1137-1142）：DSH 正在跑时
/// **不**去动它正读着的 profile，只写 pending，由进入 DSH 的钩子补注册。
///
/// 部分失败：不回滚已成功的；成功的那个从 pending 去掉，失败的那个留着；
/// 返回的文案点名是哪一个失败（方案 8.9 第 5、7 条）。
fn kb_install_blocking(app: &AppHandle) -> Result<String, String> {
    let bun = bundle_dir(&config::config_dir(app));
    release_bundles(&bun)?;
    let released_line = i18n::fmt("kb_released", &[&bun.to_string_lossy()]);
    process::setup_progress(app, "deploy", &released_line);
    log_line(app, &released_line);

    let profile = resolve_profile(app);
    if !crate::py_bridge::profile_name_usable(&profile) {
        // 保留 profile `desktop` 由 CLI 明文拒绝，这里提前拦（照 office / py_bridge）
        return Err(i18n::fmt("kb_bad_profile", &[&profile]));
    }

    // DSH 在跑：两个包都还欠着注册 → 留完整 pending，交棒给进门自愈
    if crate::py_bridge::dsh_port_busy(app) {
        let both: Vec<String> = KB_PACKAGES.iter().map(|(p, _)| p.to_string()).collect();
        write_pending_list(app, &both)?;
        let line = i18n::fmt("kb_deferred", &[&profile]);
        process::setup_progress(app, "deploy", &line);
        log_line(app, &line);
        return Ok(line);
    }

    let mut failures: Vec<String> = Vec::new();
    for (pkg, _) in KB_PACKAGES.iter() {
        // 知识层失败也继续装记忆层：两个包功能独立，装上半边总比没半边，
        // 但**绝不**因为「装成了一个」就把整体报成成功（下面按 failures 定性）。
        if let Err(e) = register_with_pending(app, &profile, pkg, &bun) {
            failures.push(e);
        }
    }

    if failures.is_empty() {
        Ok(i18n::fmt("kb_install_done", &[&profile]))
    } else {
        Err(failures.join(" / "))
    }
}

// ---------- 卸载 ----------

#[tauri::command]
pub async fn kb_uninstall(app: AppHandle) -> Result<(), String> {
    spawn_kb_task(app, "kb", kb_uninstall_blocking)
}

/// 两个包都摘（走 CLI）→ 清 pending → 删释放目录。
/// 任何一个包没摘掉都要如实报出来，不能报「已卸载」。
fn kb_uninstall_blocking(app: &AppHandle) -> Result<String, String> {
    let mut notes: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let profile = crate::py_bridge::probe_profile(app);
    match &profile {
        Some(p) => {
            for (pkg, _) in KB_PACKAGES.iter() {
                match remove_kb_package(app, p, pkg) {
                    Ok(()) => {
                        // 参数表里每个元素都要写成 `&…`：`npm run check:i18n-args` 就是
                        // 守这个的（本项目已在 E0277 上摔过三次，见那条脚本的头注）
                        let line = i18n::fmt("kb_removed", &[&pkg, &p]);
                        process::setup_progress(app, "deploy", &line);
                        notes.push(line);
                    }
                    Err(e) => {
                        let line = i18n::fmt("kb_remove_fail", &[&pkg, &p, &e]);
                        process::setup_progress(app, "deploy", &line);
                        log_line(app, &line);
                        failures.push(line);
                    }
                }
            }
        }
        None => notes.push(i18n::t("kb_no_profile").to_string()),
    }

    // 标记必须清：包已摘掉却还留着 pending，下次进入 DSH 会把两个包重新装回去
    // （pending 没清 + 摘除失败 那种「幽灵复活」路径尤其要堵死）
    clear_pending(app);
    let bun = bundle_dir(&config::config_dir(app));
    match remove_bundle_dir(&bun) {
        Ok(()) => notes.push(i18n::fmt("kb_dir_removed", &[&bun.to_string_lossy()])),
        Err((e, locked)) => {
            let key = if locked { "kb_dir_remove_locked" } else { "kb_dir_remove_fail" };
            notes.push(i18n::fmt(key, &[&bun.to_string_lossy(), &e]));
            // 目录被占住删不掉时**必须**再把里面的 tgz 删掉：自愈链的判据是
            // 「tgz 已释放 + profile 里没这个包」，留着 tgz 就等于留着「下次进入 DSH
            // 把刚卸载的东西装回来」的开关。空壳目录本身无害（下次进门清）。
            if locked {
                let gone = remove_released_files(&bun);
                if gone > 0 && !bundle_released(&bun) {
                    let line = i18n::fmt("kb_files_removed_after_lock", &[&bun.to_string_lossy()]);
                    log_line(app, &line);
                    notes.push(line);
                }
            }
        }
    }

    if failures.is_empty() {
        Ok(notes.join("\n"))
    } else {
        // 没摘干净：失败与已成功的都带回去，让用户看见真实形状
        Err([failures, notes].concat().join("\n"))
    }
}

// ---------- 进门自愈（process.rs set_status 的 running 钩子；照 office::activate_pending:1599-1727） ----------

/// pending / 已释放 / 已注册，三事实全不成立就零开销返回。
///
/// 每次**真的**走到激活都留一行日志 —— 真机事故（office，2026-11-06）：激活线程
/// 静默死亡、零日志、profile 里根本没有插件，事后完全无从排查。
pub fn activate_pending(app: &AppHandle) -> Option<String> {
    let bun = bundle_dir(&config::config_dir(app));
    let pending = pending_list(app);
    let released = bundle_released(&bun);
    let profile_hint = crate::py_bridge::probe_profile(app);
    let registered_of = |pkg: &str| -> bool {
        profile_hint
            .as_deref()
            .map(|p| crate::py_bridge::bundle_registered_named(p, pkg))
            .unwrap_or(false)
    };

    // 自愈进门（方案 8.7 的第三道防线）：pending 标记丢了也要补注册 ——
    // 判据是「tgz 已释放、profile 里却没有这个包」。
    let todo: Vec<&'static str> = KB_PACKAGES
        .iter()
        .filter(|(pkg, _)| {
            pending.iter().any(|q| q.as_str() == *pkg) || (released && !registered_of(pkg))
        })
        .map(|(pkg, _)| *pkg)
        .collect();
    if todo.is_empty() {
        // 两个包都在 profile 里了：pending 若有残留（注册成功但清标记时写盘失败）
        // 顺手清掉 —— 否则状态文案会被永久钉在「已注册…生效」那句上
        if !pending.is_empty() {
            log_line(app, &format!("两个包均已在 profile 里，清除残留 pending：{pending:?}"));
            clear_pending(app);
        }
        return None;
    }
    // 激活链的可观测性锚点（真机事故：激活线程静默死亡、零日志，事后无从排查）。
    // ⚠ 格式串里不能混用「具名内联参数」与「位置参数」（rustc 会数错参数个数），
    // 所以三个值全部走位置。
    log_line(
        app,
        &format!(
            "activate_pending：pending={:?} released={} registered({}/{})=({}/{}) 待注册={:?}",
            pending,
            released,
            KB_PKG,
            MEMORY_PKG,
            registered_of(KB_PKG),
            registered_of(MEMORY_PKG),
            todo,
        ),
    );

    // pending 在但 tgz 被清过（用户动过应用数据目录）：重新释放，别让标记永远悬空
    if !released {
        if let Err(e) = release_bundles(&bun) {
            log_line(app, &format!("重新释放 tgz 失败：{e}"));
            return Some(e);
        }
    }
    let profile = match profile_hint {
        Some(p) => p,
        None => {
            let p = resolve_profile(app);
            log_line(app, &format!("profile 探测失败，本次用兜底值 {p}"));
            p
        }
    };
    if !crate::py_bridge::profile_name_usable(&profile) {
        let line = i18n::fmt("kb_bad_profile", &[&profile]);
        log_line(app, &line);
        return Some(line);
    }

    let mut done: Vec<String> = Vec::new();
    for pkg in todo {
        // 逐个注册：一个失败不影响另一个（两个包功能独立），失败的留在 pending 里
        // 下次再试。这里**不**用 `?`：本函数跑在启动线程上，任何一步都不该
        // 中断剩下的包（宁可少装一个，也不要整条链静默断掉）。
        match register_with_pending(app, &profile, pkg, &bun) {
            Ok(()) => done.push(pkg.to_string()),
            Err(e) => log_line(app, &e),
        }
    }
    if done.is_empty() {
        None
    } else {
        Some(i18n::fmt("kb_activate_done", &[&done.join(" / "), &profile]))
    }
}

// ---------- 纯函数单测（phase / 合并文案 / pending 解析 / 释放目录） ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// 文案断言跑在中文词典上（i18n 是全局开关，本模块的用例先钉一次）
    fn zh() {
        i18n::set_lang("zh");
    }

    #[test]
    fn kb_phase_truth_table() {
        use KbPhase::*;
        assert_eq!(kb_phase(false, false), NotInstalled);
        assert_eq!(kb_phase(false, true), NeedsRelease);
        assert_eq!(kb_phase(true, false), PendingActivate);
        assert_eq!(kb_phase(true, true), Active);
    }

    #[test]
    fn kb_phase_as_str_is_the_frontend_contract() {
        // 前端拿这几个串判按钮两态，改任何一个字都是破坏性变更
        assert_eq!(KbPhase::NotInstalled.as_str(), "not-installed");
        assert_eq!(KbPhase::NeedsRelease.as_str(), "needs-release");
        assert_eq!(KbPhase::PendingActivate.as_str(), "pending-activate");
        assert_eq!(KbPhase::Active.as_str(), "active");
    }

    /// 验收第 11 条：Active 但 pending 仍在 → 「已注册进 profile，进入 DSH 后生效」，
    /// 绝不出现「已激活」。守住诚实口径本身。
    #[test]
    fn active_with_pending_does_not_claim_activated() {
        zh();
        let s = package_phrase(KbPhase::Active, true, "web");
        assert!(s.contains("已注册进 profile"), "实际文案：{s}");
        assert!(s.contains("生效"), "实际文案：{s}");
        assert!(!s.contains("已激活"), "不该谎称已激活：{s}");
        assert_eq!(package_phrase(KbPhase::NotInstalled, false, "web"), "未安装");
    }

    /// 验收第 5 条（三根柱子之一）：只装成一个 → 点名是哪一个，**不显示「已就绪」**
    #[test]
    fn partial_install_names_the_missing_layer() {
        zh();
        // 知识层装上了、记忆层没有
        let only_kb =
            kb_merged_message(KbPhase::Active, false, KbPhase::NotInstalled, false, Some("web"));
        assert!(only_kb.contains("记忆与技能层未安装"), "该点名记忆层：{only_kb}");
        assert!(!only_kb.contains("记忆与知识库已就绪"), "不得笼统报就绪：{only_kb}");
        // 反向：记忆层装上了、知识层没有（异常组合，照样如实报）
        let only_mem =
            kb_merged_message(KbPhase::NotInstalled, false, KbPhase::Active, false, Some("web"));
        assert!(only_mem.contains("知识库未安装"), "该点名知识层：{only_mem}");
        assert!(!only_mem.contains("记忆与知识库已就绪"), "不得笼统报就绪：{only_mem}");
    }

    #[test]
    fn merged_message_four_combos() {
        zh();
        let both = kb_merged_message(KbPhase::Active, false, KbPhase::Active, false, Some("web"));
        assert!(both.contains("记忆与知识库已就绪"), "实际：{both}");
        let none =
            kb_merged_message(KbPhase::NotInstalled, false, KbPhase::NotInstalled, false, None);
        assert_eq!(none, "未安装");
        // DSH 在跑时装下来的形状：两个都已释放、都还没注册 → 「进入 DSH 后生效」
        let deferred = kb_merged_message(
            KbPhase::PendingActivate,
            true,
            KbPhase::PendingActivate,
            true,
            Some("web"),
        );
        assert!(deferred.contains("进入 DSH 后生效"), "实际：{deferred}");
        assert!(!deferred.contains("已就绪"), "没注册不得说就绪：{deferred}");
    }

    /// 任一 Active 但 pending 还挂着 → 合并行也必须走「已注册…生效」口径
    #[test]
    fn merged_message_keeps_honest_wording_for_pending_active() {
        zh();
        let s = kb_merged_message(KbPhase::Active, true, KbPhase::Active, true, Some("web"));
        assert!(!s.contains("记忆与知识库已就绪"), "pending 未清不得报整体就绪：{s}");
        assert!(s.contains("已注册进 profile"), "实际：{s}");
        // 兜底行按**包名**各说一句，两个都要出现
        assert!(s.contains("dsh-kb") && s.contains("dsh-memory2skill"), "两个包都要点名：{s}");
        // 半边已注册、半边 pending 也没清：同样逐包点名，不合并成一句好话
        let mix = kb_merged_message(KbPhase::Active, true, KbPhase::NotInstalled, false, Some("web"));
        assert!(mix.contains("未安装"), "实际：{mix}");
        assert!(mix.contains("dsh-kb") && mix.contains("dsh-memory2skill"), "实际：{mix}");
        assert!(!mix.contains("记忆与知识库已就绪"), "实际：{mix}");
        // NeedsRelease（profile 里有、tgz 被清过）也走逐包点名
        let needs = kb_merged_message(KbPhase::NeedsRelease, false, KbPhase::Active, false, Some("web"));
        assert!(needs.contains("dsh-kb"), "实际：{needs}");
        assert!(!needs.contains("记忆与知识库已就绪"), "实际：{needs}");
    }

    #[test]
    fn embedded_bundles_match_package_table() {
        // 两张表必须一一对应：改了 tgz 名忘了改 include_bytes! 路径会在这里断掉
        let names: Vec<&str> = KB_BUNDLE_FILES.iter().map(|(n, _)| *n).collect();
        let want: Vec<&str> = KB_PACKAGES.iter().map(|(_, f)| *f).collect();
        assert_eq!(names, want);
        // 嵌进二进制的字节不能是空（构建期资源没带上是最坏的一种「装了却装不上」），
        // 而且必须是 gzip（tgz 的头两字节 = 1f 8b）
        for (name, body) in KB_BUNDLE_FILES.iter() {
            assert!(body.len() > 1024, "{name} 嵌进来的内容太小：{}", body.len());
            assert_eq!(&body[..2], &[0x1f, 0x8b], "{name} 不是 gzip 包");
        }
    }

    #[test]
    fn install_order_is_knowledge_first() {
        // 知识层是主功能，必须排第一（状态文案的主次跟着它）
        assert_eq!(KB_PACKAGES[0].0, KB_PKG);
        assert_eq!(KB_PACKAGES[1].0, MEMORY_PKG);
        assert_eq!(KB_PKG, "dsh-kb");
        assert_eq!(MEMORY_PKG, "dsh-memory2skill");
    }

    /// pending 列表的纯字符串部分：join / split 往返 + 「只去掉成功的那个」
    #[test]
    fn pending_list_shape_and_partial_clear() {
        let list = vec![KB_PKG.to_string(), MEMORY_PKG.to_string()];
        let text = list.join(",");
        assert_eq!(text, "dsh-kb,dsh-memory2skill");
        let back: Vec<String> = text
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(back, list);
        // 部分失败：成功的那个去掉，失败的那个留下等下次自愈
        let kept: Vec<String> = back.iter().filter(|p| p.as_str() != KB_PKG).cloned().collect();
        assert_eq!(kept, vec![MEMORY_PKG.to_string()]);
        // 空串 / 全空白 = 没有 pending（清除标记写的是空串，不是删键）
        for raw in ["", "   ", ",", " , "] {
            let parsed: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            assert!(parsed.is_empty(), "「{raw}」不该解析出 pending 包名");
        }
    }

    #[test]
    fn bundle_dir_and_release_are_idempotent() {
        let base = std::env::temp_dir().join(format!("dsh-kb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let bun = bundle_dir(&base);
        assert_eq!(bun, base.join(BUNDLE_DIR_NAME));
        // 目录不存在 → 未释放
        assert!(!bundle_released(&bun));
        // 只有一个包 → 仍算未释放（半份包不算，与 office 同一口径）
        std::fs::create_dir_all(&bun).unwrap();
        std::fs::write(bun.join(KB_TGZ), b"x").unwrap();
        assert!(!bundle_released(&bun));
        // 两个都在 → 已释放
        std::fs::write(bun.join(MEMORY_TGZ), b"x").unwrap();
        assert!(bundle_released(&bun));
        // 幂等重释放：走原子改名，内容以嵌进二进制的 tgz 为准（上面写的 b"x" 被覆盖）
        release_bundles(&bun).unwrap();
        assert!(bundle_released(&bun));
        assert!(std::fs::metadata(bun.join(KB_TGZ)).unwrap().len() > 1024);
        // 临时文件不留残渣
        let leftovers: Vec<_> = std::fs::read_dir(&bun)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "释放留下临时文件：{leftovers:?}");
        // 删目录（释放目录没被锁时应当成功）
        remove_bundle_dir(&bun).ok();
        // 空壳目录判据（cleanup_stale_kb_dirs 靠它）
        std::fs::create_dir_all(&bun).unwrap();
        assert!(std::fs::read_dir(&bun).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 卸载时目录被占住 → 必须把里面的 tgz 删光，否则自愈链会把刚卸载的插件装回来。
    /// （这条守的是「卸载 = 真的卸载」，不是目录外观干净与否。）
    #[test]
    fn locked_dir_stops_self_heal_by_emptying_it() {
        let base = std::env::temp_dir().join(format!("dsh-kb-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let bun = bundle_dir(&base);
        release_bundles(&bun).unwrap();
        assert!(bundle_released(&bun), "前置条件：两个包都已释放");
        // 逐个删掉 tgz（就是 remove_bundle_dir 失败时卸载走的那条补删路径）
        assert_eq!(remove_released_files(&bun), 2, "两个包都该被删掉");
        assert!(!bundle_released(&bun), "删光之后必须判为「未释放」，自愈才不会装回来");
        // 目录还在（空壳）—— 这正是 cleanup 的判据
        assert!(bun.is_dir());
        assert!(std::fs::read_dir(&bun).unwrap().next().is_none(), "空壳应当没有条目");
        // 再补删一次：幂等，返回 0 而不是报错
        assert_eq!(remove_released_files(&bun), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_cleanup_keeps_non_empty_dir() {
        // 这条只验判据本身（非空 = 有效的安装包，不动）：
        // cleanup_stale_kb_dirs 需要 AppHandle，这里复用它读的同一个条件
        let base = std::env::temp_dir().join(format!("dsh-kb-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("keep.txt"), b"x").unwrap();
        assert!(std::fs::read_dir(&base).unwrap().next().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}
