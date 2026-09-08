use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

use crate::{detect, i18n};

/// Launcher 的持久化配置，保存于 %APPDATA%\com.dsh.desktop\config.json
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// npm.cmd 完整路径（用于更新 DSH / 查询最新版本）；留空或失效时自动检测
    pub npm_path: String,
    /// dsh.cmd 完整路径（用于启动 DSH）；留空或失效时自动检测
    pub dsh_path: String,
    /// DSH 家目录：DSH 摆放配置文件的地方（通过 DSH_HOME 环境变量传给 DSH）
    /// 启动进程的工作目录自动取其上一级目录；DSH 的工作区在网页内随意指定
    pub dsh_home_dir: String,
    /// DSH Web 服务端口（默认 3080，可在设置页修改，1~65535）
    pub port: u16,
    /// 附加启动参数（空格分隔，追加在 `dsh web --port <port> --no-open` 之后）
    pub extra_args: String,
    /// DSH 的 npm 包名（用于 `npm view <name> dist-tags` 与更新命令）
    pub package_name: String,
    /// 等待 DSH 就绪的超时时间（秒）；0 = 一直等待（只要 DSH 进程还活着）
    pub health_timeout_secs: u64,
    /// 点击主窗口 X 时的行为："tray" = 隐藏到托盘（默认），"quit" = 退出程序
    pub close_action: String,
    /// 界面语言："zh" = 中文（默认），"en" = English。
    /// 保存时同步写入 DSH 家目录 settings.yaml 的 locale.preference，
    /// 让 DSH 自身的 Web 界面跟随中英文切换。
    pub language: String,
    /// 外观："light" = 浅色，"dark" = 深色，"system" = 跟随系统（默认）。
    /// 同时控制桌面端界面（前端 data-theme + 原生标题栏 set_theme）与 DSH Web 界面：
    /// 保存时写入 settings.yaml 的 ui-theme.preference，DSH 的 settings-file 提供器
    /// 监视该文件并把变化实时推送到已打开的页面，无需重启 DSH。
    /// 老配置（无此字段）首次加载时继承 DSH settings.yaml 里的现有主题，避免升级即覆盖。
    pub appearance: String,
    /// 最近一次交给内嵌 WebView 的 DSH 页面完整地址（next 频道带 `?token=...`）。
    /// 这是程序自己写的运行时记忆（不是用户设置），只用于「连接现有服务 / 页面重开」
    /// 这类没有新进程输出的场景；读取侧（process.rs）每次使用前重新校验形状与端口。
    pub last_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 不硬编码任何个人路径；加载时按本机环境自动检测填充
            npm_path: String::new(),
            dsh_path: String::new(),
            dsh_home_dir: default_dsh_home_dir(),
            port: 3080,
            extra_args: String::new(),
            package_name: "@deepseek-ai/dsh".to_string(),
            // DSH 冷启动（尤其重启电脑后首次）可能需要 1~2 分钟以上，默认给足 5 分钟
            health_timeout_secs: 300,
            // 默认点 X 隐藏到托盘（后台继续运行）；可在设置页改为退出程序
            close_action: "tray".to_string(),
            // 默认中文界面
            language: "zh".to_string(),
            // 默认跟随系统外观（DSH 的 ui-theme 默认值也是 system，两边一致）
            appearance: "system".to_string(),
            last_url: String::new(),
        }
    }
}

fn default_dsh_home_dir() -> String {
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    PathBuf::from(base)
        .join(".dsh")
        .to_string_lossy()
        .to_string()
}

/// DSH / npm 进程的工作目录：家目录的上一级（如 C:\Users\<你>\.dsh -> C:\Users\<你>）。
///
/// 这里刻意做校验而不是返回裸字符串：cwd 决定 DSH（一个带文件/shell 工具的 Agent）
/// 从哪一层开始观察，也决定 npm 去哪个目录读 `./.npmrc`（一个 `.npmrc` 里的
/// `registry=` 就能把后续所有安装流量导向攻击者的源）。所以「家目录被改成别的盘/
/// 别的目录」不只是路径难看，而是能改变执行语义 —— 校验失败必须报错，不能静默兜底。
pub fn workspace_cwd(cfg: &Config) -> Result<String, String> {
    cwd_of_home(&cfg.dsh_home_dir)
}

/// 同上，但输入已是家目录串（便于校验后复用同一个规范化值）。
pub fn cwd_of_home(home_dir: &str) -> Result<String, String> {
    let home = validate_home_dir(home_dir)?;
    let parent = PathBuf::from(&home)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string())
        });
    // 家目录就贴在驱动器根下面（如 D:\dsh）时，父级就是驱动器根本身：
    // 让 DSH / npm 的 cwd 落在 `D:\` 会白送整个盘根的文件清单，
    // 但为此拒绝用户把家目录放在 `D:\dsh` 这种常见位置也不合理 —— 于是退一步，
    // 用家目录自己当工作目录（比原行为严格更安全，也不打扰正常用法）。
    if std::path::Path::new(&parent).components().count() <= 2 {
        return Ok(home);
    }
    Ok(parent)
}

// ---------- 路径安全策略（MEDIUM-3） ----------
//
// 为什么必须校验：配置里的这几个路径不是「只是个字符串」，它们各自是一个能力：
// - dsh_home_dir → DSH_HOME 环境变量、DSH 进程工作目录的父级（workspace_cwd）、
//   create_dir_all + settings.yaml 写入位置（本文件 sync_dsh_locale）、
//   以及 <home>\logs\dsh.log 的镜像写入位置（logger.rs）。
//   合起来就是「在家目录之外没有任何约束的任意目录创建 + 写文件」原语，
//   而且 cwd 落在哪里还决定了 DSH（一个有文件/shell 工具的 Agent）能看到什么。
// - dsh_path / npm_path → 实际被执行的文件。
// 而 config.json 是 %APPDATA% 下的明文文件，任何以本用户身份运行的进程都能写
// （恶意 npm postinstall、被提示词注入诱导而改了文件的 DSH 自己……），
// 程序又会在开机自启时静默按它执行 —— 所以「只在保存时校验」是不够的。
//
// 策略是黑名单式的（不是「必须在用户目录内」）：本仓库开发者的工作区就在 D:\，
// 强行白名单会误伤。规则：绝对路径、非 UNC、无 `..`、不落进系统/程序目录、
// 可执行文件还要「存在 + 扩展名白名单 + 不放临时目录」。

/// 大小写无关地判断 `p`（已规范化、小写）是否等于 `root` 或位于其下。
/// 按分隔符边界比较，避免 `C:\WEBSITE` 被判定在 `C:\W` 之下这类误判。
fn is_under(p_lower: &str, root_lower: &str) -> bool {
    if root_lower.is_empty() {
        return false;
    }
    p_lower == root_lower
        || (p_lower.starts_with(root_lower)
            && p_lower.as_bytes().get(root_lower.len()) == Some(&b'\\'))
}

/// 取环境变量并规范化成小写、去尾分隔符的形式；缺失或为空返回 None。
fn env_lower(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim_end_matches(|c| c == '\\' || c == '/').to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

/// 不允许作为「家目录 / 日志与配置写入点」的系统位置。
/// 注意：程序路径不受此限制 —— Node.js 官方就装在 `C:\Program Files\nodejs\`。
fn system_roots() -> Vec<String> {
    let mut v: Vec<String> = ["SystemRoot", "windir", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"]
        .iter()
        .filter_map(|k| env_lower(k))
        .collect();
    // 环境变量被清空/异常时的兜底：至少挡住最常识性的两个位置
    if v.is_empty() {
        v.push("c:\\windows".to_string());
        v.push("c:\\program files".to_string());
    }
    v
}

/// 形状校验：绝对路径、非 UNC、无 `..`，并返回规范化（统一 `\`、去掉 `.` 与重复分隔符）
/// 后的字符串。用 Path::components 完成，避免自己写字符串拼接出错的分支。
fn path_shape(field: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(i18n::fmt("err_path_empty", &[&field]));
    }
    // UNC：\\ 或 // 开头。写向远程共享会带出 NTLM 认证（凭据外泄/中继面），
    // 从远程共享加载可执行文件则在未强制 SMB 签名时等于把二进制交给网络对端。
    let flat = trimmed.replace('/', "\\");
    if flat.starts_with("\\\\") {
        return Err(i18n::fmt("err_path_unc", &[&field]));
    }
    let path = std::path::Path::new(trimmed);
    if !path.is_absolute() {
        return Err(i18n::fmt("err_path_relative", &[&field]));
    }
    let mut out = String::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(p) => {
                out.push_str(&p.as_os_str().to_string_lossy().replace('/', "\\"))
            }
            std::path::Component::RootDir => out.push('\\'),
            std::path::Component::CurDir => {} // 丢掉 `.`
            std::path::Component::ParentDir => {
                // `..` 在校验时不该出现：先规范化再判断才是可靠的，
                // 所以这里直接拒绝，而不是折叠掉它（折叠会把 `C:\Windows\..\x` 洗白）
                return Err(i18n::fmt("err_path_traversal", &[&field]));
            }
            std::path::Component::Normal(n) => {
                // RootDir 已经补过一个 `\`，这里再无条件补就会得到 `C:\\Users` ——
                // 多一个分隔符会让下面所有 is_under() 前缀比较整体失效（黑名单形同虚设），
                // 所以必须先判断末尾再补。
                if !out.ends_with('\\') {
                    out.push('\\');
                }
                out.push_str(&n.to_string_lossy().replace('/', "\\"));
            }
        }
    }
    let norm = out.trim_end_matches('\\').to_string();
    if norm.is_empty() || norm.chars().all(|c| c == '\\') {
        return Err(i18n::fmt("err_path_root", &[&field]));
    }
    Ok(norm)
}

/// 校验「DSH 家目录」。返回规范化后的路径（调用方应使用返回值，不要再用原始串）。
pub fn validate_home_dir(raw: &str) -> Result<String, String> {
    let field = "dsh_home_dir";
    let norm = path_shape(field, raw)?;
    let lower = norm.to_ascii_lowercase();

    // 驱动器根（`C:` / `C:\`）与用户目录本身：会让 cwd 退化成 C:\ 或 C:\Users，
    // 等于把 DSH（一个带文件/shell 工具的 Agent）的工作目录推到能看见全体用户资料的位置。
    if lower.len() <= 2 || !lower.contains('\\') {
        return Err(i18n::fmt("err_path_root", &[&field]));
    }
    if let Some(prof) = env_lower("USERPROFILE") {
        // is_under(prof, lower) == lower 是 USERPROFILE 本身或它的祖先目录
        if is_under(&prof, &lower) {
            return Err(i18n::fmt("err_path_root", &[&field]));
        }
    }
    for root in system_roots() {
        if is_under(&lower, &root) {
            return Err(i18n::fmt("err_path_system", &[&field]));
        }
    }
    Ok(norm)
}

/// 校验「要执行的程序路径」的形状与位置：绝对路径、非 UNC、无 `..`、
/// 扩展名白名单、且不放在临时目录（临时目录是恶意软件的标准落点，
/// 也是可预测路径竞争的现场）。**不要求文件存在** —— 允许用户先把路径填好、
/// 之后再去装 Node/DSH（保存入口用这个，执行点用 validate_program_file）。
/// `field` 只用于报错文案，让 dsh_path / npm_path 各自报自己的名字。
pub fn validate_program_shape(field: &str, raw: &str) -> Result<String, String> {
    let norm = path_shape(field, raw)?;
    let ext = std::path::Path::new(&norm)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // npm 还会生成同名无扩展 sh 脚本与 .ps1：.ps1 无法直接 CreateProcess，
    // 且会走「文件关联 + 执行策略」这条不可控的路，所以一并排除在白名单外。
    if !matches!(ext.as_str(), "exe" | "cmd" | "bat") {
        return Err(i18n::fmt("err_prog_ext", &[&norm]));
    }
    let lower = norm.to_ascii_lowercase();
    for var in ["TEMP", "TMP"] {
        if let Some(t) = env_lower(var) {
            if is_under(&lower, &t) {
                return Err(i18n::fmt("err_path_temp", &[&norm]));
            }
        }
    }
    Ok(norm)
}

/// 执行点用：形状合法 + 确实是一个存在的普通文件。
pub fn validate_program_file(field: &str, raw: &str) -> Result<String, String> {
    let norm = validate_program_shape(field, raw)?;
    let md = std::fs::metadata(&norm).map_err(|_| i18n::fmt("err_prog_missing", &[&norm]))?;
    if !md.is_file() {
        return Err(i18n::fmt("err_prog_missing", &[&norm]));
    }
    Ok(norm)
}

/// 外观值归一化：只接受 light / dark / system，其余（含手改的非法值）回落 system。
pub fn normalize_appearance(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    }
}

/// 把界面语言写入 `<DSH 家目录>\settings.yaml` 的 locale.preference。
pub fn sync_dsh_locale(home_dir: &str, language: &str) -> Result<(), String> {
    let pref = if language.eq_ignore_ascii_case("en") { "en" } else { "zh" };
    sync_dsh_setting(home_dir, "locale", pref)
}

/// 把外观写入 `<DSH 家目录>\settings.yaml` 的 ui-theme.preference。
/// DSH 的 settings-file 提供器用文件监视器热加载该文件并把变化推送给已打开的
/// 页面（客户端 ThemeRuntime 订阅 settings scope），因此无需重启 DSH 即可换肤。
pub fn sync_dsh_theme(home_dir: &str, appearance: &str) -> Result<(), String> {
    sync_dsh_setting(home_dir, "ui-theme", normalize_appearance(appearance))
}

/// 把 `preference: <pref>` 写入 `<DSH 家目录>\settings.yaml` 的 `<block>:` 块
/// （最小侵入式行编辑，locale 与 ui-theme 共用）。
/// - 已有 `<block>:` 块与 `preference:` 行 → 仅替换该行；
/// - 有 `<block>:` 块但没有 preference → 在块首插入；
/// - 完全没有 → 文件末尾追加 `<block>:\n  preference: <pref>` 块；
/// - 文件不存在 → 创建仅含该块的新文件。
/// 其余行（含 ui-theme 块的 fontSize 等同级键）原样保留，不引入 YAML 解析依赖。
fn sync_dsh_setting(home_dir: &str, block: &str, pref: &str) -> Result<(), String> {
    // 这里是一个真实的「建目录 + 写文件」出口，而且 set_language 命令会带着
    // 磁盘上读来的 home_dir 直接走到这里（没经过保存入口），所以在此独立校验。
    let dir = PathBuf::from(validate_home_dir(home_dir)?);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let path = dir.join("settings.yaml");
    let content = if path.is_file() {
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?
    } else {
        String::new()
    };

    let block_key = format!("{block}:");
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    let mut found_block = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        // 顶层 `<block>:` 键（行首无缩进、去掉尾部空白后恰为该键）
        if line.trim_end() == block_key.as_str() && !line.starts_with(' ') && !line.starts_with('\t') {
            found_block = true;
            out.push(line.to_string());
            i += 1;
            // 收集该键的缩进子块（含空行/注释），并在其中替换/插入 preference
            let mut sub: Vec<String> = Vec::new();
            let mut replaced = false;
            while i < lines.len() {
                let bl = lines[i];
                if bl.trim().is_empty() {
                    sub.push(bl.to_string());
                    i += 1;
                    continue;
                }
                if !bl.starts_with(' ') && !bl.starts_with('\t') {
                    break; // 到达下一个顶层键
                }
                if bl.trim_start().starts_with("preference:") {
                    sub.push(format!("  preference: {}", pref));
                    replaced = true;
                } else {
                    sub.push(bl.to_string());
                }
                i += 1;
            }
            if !replaced {
                sub.insert(0, format!("  preference: {}", pref));
            }
            out.extend(sub);
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    if !found_block {
        if !out.is_empty() && !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            out.push(String::new());
        }
        out.push(block_key);
        out.push(format!("  preference: {}", pref));
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(())
}

/// 从 `<DSH 家目录>\settings.yaml` 的 ui-theme 块读取 preference（light/dark/system）。
/// 仅用于「老配置没有 appearance 字段时继承 DSH 现有主题」：文件缺失、块缺失、
/// 值非法一律返回 None（调用方保持自己的默认值），绝不在此处写盘。
pub fn read_dsh_theme(home_dir: &str) -> Option<String> {
    let home = home_dir.trim();
    if home.is_empty() {
        return None; // 未配置家目录：绝不回退到相对路径去读进程 CWD 里的同名文件
    }
    let path = PathBuf::from(home).join("settings.yaml");
    let content = std::fs::read_to_string(&path).ok()?;
    let mut in_block = false;
    for line in content.lines() {
        let trimmed = line.trim_end();
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_block = trimmed == "ui-theme:";
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("preference:") {
            let value = rest.trim().trim_matches(|c| c == '\'' || c == '"').trim();
            return match normalize_appearance(value) {
                // settings.yaml 里写了认不出的值：按无现有主题处理，不继承
                "system" if !value.eq_ignore_ascii_case("system") => None,
                v => Some(v.to_string()),
            };
        }
    }
    None
}

/// 用自动检测结果补全缺失/失效的路径。
/// 仅在内存中生效，不回写配置文件——用户手动保存过的有效路径永远优先。
fn autofill_from_detection(cfg: &mut Config) {
    let detected = detect::detect_all(false);
    let npm_missing = cfg.npm_path.trim().is_empty() || !PathBuf::from(&cfg.npm_path).is_file();
    if npm_missing {
        if let Some(p) = detected.npm.as_ref() {
            cfg.npm_path = p.to_string_lossy().to_string();
        }
    }
    let dsh_missing = cfg.dsh_path.trim().is_empty() || !PathBuf::from(&cfg.dsh_path).is_file();
    if dsh_missing {
        if let Some(p) = detected.dsh.as_ref() {
            cfg.dsh_path = p.to_string_lossy().to_string();
        }
    }
}

pub fn config_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from(".dsh-desktop"))
}

pub fn config_path(app: &AppHandle) -> PathBuf {
    config_dir(app).join("config.json")
}

/// 旧版本（identifier 为 com.dsh.launcher）遗留的配置文件路径，仅用于一次性迁移
fn legacy_config_path() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(base).join("com.dsh.launcher").join("config.json"))
}

pub fn load(app: &AppHandle) -> Config {
    let path = config_path(app);
    let mut first_run = false;
    let mut has_appearance = false;
    let mut cfg = if let Ok(s) = std::fs::read_to_string(&path) {
        // 老版本 config.json 没有 appearance 字段：先探一下键是否存在（合法字符串值），
        // 缺失时下面再从 DSH settings.yaml 继承现有主题，避免升级即把 DSH 页面改色。
        has_appearance = serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .map(|v| v.get("appearance").and_then(|a| a.as_str()).is_some())
            .unwrap_or(false);
        serde_json::from_str::<Config>(&s).unwrap_or_default()
    } else {
        // 自动迁移旧目录 com.dsh.launcher → com.dsh.desktop，避免升级后配置丢失
        first_run = true;
        let mut migrated = None;
        if let Some(old) = legacy_config_path() {
            if old != path {
                if let Ok(s) = std::fs::read_to_string(&old) {
                    if let Ok(c) = serde_json::from_str::<Config>(&s) {
                        migrated = Some(c);
                    }
                }
            }
        }
        match migrated {
            // 迁移：旧配置必然没有 appearance 字段——先从 DSH settings.yaml 继承
            // 现有主题，再写盘，保证磁盘与内存一致（否则下次加载读到 system 会翻回去）
            Some(mut c) => {
                if let Some(t) = read_dsh_theme(&c.dsh_home_dir) {
                    c.appearance = t;
                }
                let _ = save(app, &c);
                has_appearance = true;
                c
            }
            // 全新环境：不写盘（向导 finish_setup 负责落盘），继承逻辑走下方统一分支
            None => Config::default(),
        }
    };
    // 首次运行（config.json 尚未生成）时，向导「选择语言」一步的选择存在
    // ui-language sidecar 里；这里取它作为界面语言，等 finish_setup 真正
    // 写出 config.json 后就以其 language 字段为准（sidecar 随 save 一并清除）。
    if first_run {
        if let Some(lang) = read_ui_language_override(app) {
            cfg.language = lang;
        }
    }
    // 缺失/失效的路径用本机检测结果补齐（不写盘，写盘仍由用户「保存」触发）
    autofill_from_detection(&mut cfg);
    // 升级兼容：config.json 里没有 appearance 字段（老版本首次升级、或首次运行
    // 尚未走完向导）时，继承 DSH settings.yaml 的现有主题；读不到则保持默认 system。
    // 这样升级桌面端不会把用户已经在 DSH 页面里选好的外观改掉。
    if !has_appearance {
        if let Some(t) = read_dsh_theme(&cfg.dsh_home_dir) {
            cfg.appearance = t;
        }
    }
    cfg
}

pub fn save(app: &AppHandle, cfg: &Config) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| {
        i18n::fmt("err_cfg_dir", &[&dir.display().to_string(), &e.to_string()])
    })?;
    let path = config_path(app);
    let s = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| {
        i18n::fmt("err_cfg_write", &[&path.display().to_string(), &e.to_string()])
    })?;
    // config.json 一旦存在，语言以其中的 language 字段为准，sidecar 完成使命
    let _ = std::fs::remove_file(ui_language_sidecar_path(app));
    Ok(())
}

// ---------- 最近加载地址记忆（last_url） ----------
//
// last_url 只记录「最近一次成功交给内嵌 WebView 的 DSH 完整地址」（可能带
// `?token=...` 会话令牌）。它不走 Config 结构体：不经过自动检测填充、不随
// 用户保存整体写盘，避免把内存态路径固化；写入采用读取-修改-写回，只动这一个
// 键。读取侧每次使用前在 process.rs 里重新校验形状与端口，被手改/损坏的
// 记录会被忽略——它最多影响「连接现有服务 / 页面重开」时加载哪个本机地址，
// 那个 webview 没有任何 capability（remote origin + 无权限），不存在提权面。

/// 只更新 config.json 里的 last_url 字段（读取-修改-写回），失败静默。
pub fn set_last_url(app: &AppHandle, url: &str) {
    let path = config_path(app);
    let Ok(s) = std::fs::read_to_string(&path) else { return };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&s) else { return };
    root["last_url"] = serde_json::Value::String(url.to_string());
    if let Ok(text) = serde_json::to_string_pretty(&root) {
        let _ = std::fs::write(&path, text);
    }
}

/// 读取 last_url 原始字符串；缺失/非字符串/文件异常一律返回空串（校验在 process.rs）。
pub fn last_url(app: &AppHandle) -> String {
    let path = config_path(app);
    let Ok(s) = std::fs::read_to_string(&path) else { return String::new() };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&s) else { return String::new() };
    root["last_url"].as_str().unwrap_or("").to_string()
}

// ---------- 界面语言 sidecar（首次运行向导专用） ----------

/// sidecar 路径：<config 目录>\ui-language（内容只有 "zh" / "en" 一行）。
/// 仅在 config.json 尚不存在（first_run）期间生效：向导第一步选完语言立即
/// 持久化，中途中断重开也不会丢；finish_setup 写出 config.json 后即失效。
fn ui_language_sidecar_path(app: &AppHandle) -> PathBuf {
    config_dir(app).join("ui-language")
}

/// 记录向导里选择的语言（写入失败由调用方记录日志，不阻断界面切换）。
pub fn set_ui_language_override(app: &AppHandle, lang: &str) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let value = if lang.eq_ignore_ascii_case("en") { "en" } else { "zh" };
    std::fs::write(ui_language_sidecar_path(app), value).map_err(|e| e.to_string())
}

/// 读取 sidecar；内容非法（被手改）时忽略，回落默认 zh。
fn read_ui_language_override(app: &AppHandle) -> Option<String> {
    let raw = std::fs::read_to_string(ui_language_sidecar_path(app)).ok()?;
    let t = raw.trim();
    if t.eq_ignore_ascii_case("en") {
        Some("en".to_string())
    } else if t.eq_ignore_ascii_case("zh") {
        Some("zh".to_string())
    } else {
        None
    }
}
