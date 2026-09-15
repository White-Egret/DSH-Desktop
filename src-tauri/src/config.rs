use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

use crate::{detect, i18n, secret};

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
    /// 最近一次交给内嵌 WebView 的 DSH 页面完整地址，**以 DPAPI 密文（十六进制）存放**
    /// （地址可能带 `?token=<base64url>` 会话令牌，明文绝不落盘 —— 安全审查 MEDIUM-2，
    /// 加解密原语见 secret.rs）。这是程序自己写的运行时记忆（不是用户设置），只用于
    /// 「连接现有服务 / 页面重开」这类没有新进程输出的场景；读取侧（process.rs）
    /// 每次使用前重新校验形状与端口。
    /// 字段名带 `_enc` 是刻意的：读代码/读配置文件的人一眼就知道它不是一个能直接用的地址。
    pub last_url_enc: String,
    /// 用户「保留过低版本 Node 并继续」的决定记录：内容是当时确认过的**最低版本**
    /// （如 `22.19.0`）。为空 = 没确认过；与当前 NODE_MIN_VERSION 不同 = 程序把下限
    /// 提高了，需要重新问一次。取值由 remember_node_min_ack 做读取-修改-写回，
    /// 不经过设置页，避免把「一次性确认」变成用户要维护的配置项。
    pub node_min_ack: String,
    /// 安全模式：进入前是否重置环境基线（默认 false = 不重置，沿用已有安全环境）。
    /// 开启时若 `%USERPROFILE%\.dsh-safe` 已存在，整目录重命名为
    /// `.dsh-safe-archive-<YYYYMMDD-HHMMSS>` 归档后重建空目录，
    /// 保证每次进入安全模式都是「除借用的凭据外全空」的原厂状态；
    /// 关闭（默认）时沿用已有 .dsh-safe —— 上一轮安全模式的配置与日志得以保留，
    /// 但凭据文件仍会每次覆盖拷贝为当前有效版本。
    /// 首次进入（目录还不存在）时两条路径结果相同：都是只含凭据的空家目录。
    pub safe_reset_baseline: bool,
    /// 安全模式：修复验证等待秒数（默认 80）。退出安全模式并按日常路径重启后，
    /// 日常实例在该时间内未就绪则提示「修复可能未成功」并提供返回安全模式入口。
    /// 0 = 不做验证提示。保存时非 0 值会被收敛到 5~3600 秒（与就绪超时同一刻度）。
    pub safe_verify_secs: u64,
    /// 工具栏模式："pinned" = 固定显示（默认，等同历史行为）；"auto" = 自动隐藏
    /// （鼠标移到窗口顶部约 8px 的触发条时滑出，移开工具栏半秒后收起）。
    /// 这里只存**用户偏好**：实际生效判定在 process.rs（`content_offset_for`），
    /// 且只在日常模式生效 —— 安全模式强制固定显示，因为安全模式的退出按钮与
    /// 琥珀色徽标就长在工具栏上，把它藏起来等于把用户困在安全模式里。
    pub toolbar_mode: String,
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
            last_url_enc: String::new(),
            // 空 = 还没在「Node 版本过低」告警里选过「保留该版本继续」
            node_min_ack: String::new(),
            // 安全模式默认**不**重置基线：沿用已有 .dsh-safe，上一轮安全模式的
            // 配置与日志得以保留；想每次进入都回到原厂基线可在首选项里开启
            safe_reset_baseline: false,
            // 修复验证默认等 80 秒
            safe_verify_secs: 80,
            // 工具栏默认固定显示（= 历史行为）：升级后不会突然变成"工具栏不见了"
            toolbar_mode: "pinned".to_string(),
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

/// 工具栏模式归一化：只接受 pinned / auto，其余（含老配置缺字段、手改的非法值）
/// 一律回落 **pinned** —— 那是历史行为，升级后工具栏不会莫名消失。
/// 顺手接受 auto-hide / auto_hide 这类等价写法，免得前端换个拼法就被静默忽略。
pub fn normalize_toolbar_mode(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "auto" | "auto-hide" | "autohide" => "auto",
        _ => "pinned",
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
    // 原子替换（L-6）：settings.yaml 是 DSH 自己在读的文件，半截内容会被它读进去
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {}", path.display(), e))?;
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
    // 明文 last_url 迁移（MEDIUM-2）：≤1.2.5 把带 ?token= 的完整地址明文写在
    // `last_url` 键里。这里先把它取出来（只在还没有密文时），函数末尾再加密写回去，
    // 并把明文键删掉 —— 取出即加密，磁盘上不留明文副本。
    let mut legacy_last_url: Option<String> = None;
    let mut cfg = if let Ok(s) = std::fs::read_to_string(&path) {
        // 老版本 config.json 没有 appearance 字段：先探一下键是否存在（合法字符串值），
        // 缺失时下面再从 DSH settings.yaml 继承现有主题，避免升级即把 DSH 页面改色。
        let raw = serde_json::from_str::<serde_json::Value>(&s).ok();
        has_appearance = raw
            .as_ref()
            .map(|v| v.get("appearance").and_then(|a| a.as_str()).is_some())
            .unwrap_or(false);
        // 只在「还没有密文」时才认这个明文键：非空即迁移，空串/非字符串当作没有
        let has_enc = raw
            .as_ref()
            .and_then(|v| v.get("last_url_enc"))
            .and_then(|v| v.as_str())
            .map(|h| !h.trim().is_empty())
            .unwrap_or(false);
        if !has_enc {
            legacy_last_url = raw
                .as_ref()
                .and_then(|v| v.get("last_url"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|u| !u.trim().is_empty());
        }
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
    // 明文 last_url 的迁移落到盘上：只动 `last_url` / `last_url_enc` 两个键，
    // **不整文件重写** —— 否则会把上面自动检测填好的路径顺手固化进用户配置
    //（autofill_from_detection 的约定是「只在内存生效，写盘仍由用户保存触发」）。
    // 加密失败时什么都不写：保留旧键让本次运行仍能复用地址，下次启动再试，
    // 绝不给明文换一个键名再存一遍。
    if let Some(url) = legacy_last_url {
        let _ = migrate_plaintext_last_url(app, &url);
    }
    cfg
}

// ---------- 原子写盘（安全审查 L-6） ----------
//
// 这些文件以前一律用 `fs::write` 直接覆盖，而它是「打开 + 截断 + 写入」三步：
// 崩溃 / 断电 / 被杀进程恰好发生在这中间时，磁盘上留下的是**半截**文件。
// 加载侧对 config.json 用的是 `serde_json::from_str(...).unwrap_or_default()`，
// 读到半截 JSON 就静默回落默认值 —— 用户一次崩溃就换来「所有设置被抹掉」
// （不构成提权，但属于会真实发生的数据丢失）；settings.yaml 那侧更麻烦，
// 因为我们只做「最小行编辑」，半截 YAML 会被 DSH 自己读进去。

/// 同一进程内保证每次调用用的临时文件名都不同（跨进程靠 pid 区分）。
static TMP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 临时文件路径：与目标**同目录**（`rename` 的原子性要求），名字带 pid 与进程内序号。
fn tmp_path_for(path: &std::path::Path, pid: u32, seq: u32) -> PathBuf {
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    path.with_file_name(format!("{}.tmp-{}-{}", stem, pid, seq))
}

/// 原子写盘：写同目录临时文件 → fsync → `rename` 覆盖目标。
///
/// 三个细节都是必要的：
/// - 临时文件必须与目标**同目录**：`rename` 只在同卷内是原子替换（`std::fs::rename`
///   跨卷时直接报错），放到 `%TEMP%` 就完全失去意义了；
/// - 用 `create_new` 而不是 `create`：目标名已存在时（可能是别人预置的符号链接）**失败**，
///   绝不顺着链接写到别处去；
/// - 先 `sync_all` 再 `rename`：否则崩溃后可能出现「目录项已指向新文件、内容还在缓存里」。
///
/// Windows 上 `std::fs::rename` 走 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，
/// 对已存在的目标是覆盖式替换；POSIX `rename(2)` 语义相同。
///
/// 注意（留给 Linux 移植）：`rename` 会换掉 inode，目标原有的权限位不会自动继承，
/// 新文件拿到的是默认模式。当前调用点写的都不是敏感内容、且 Windows 下权限由目录 ACL
/// 决定，所以无影响；将来若在 Unix 上写需要 0600 的文件，要先 set_permissions 再 rename。
pub(crate) fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = tmp_path_for(path, std::process::id(), seq);

    // 连临时文件都建不起来时，目标文件一个字节都没动 —— 正是我们要的失败语义
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let mut failed = f.write_all(contents).err();
    if failed.is_none() {
        failed = f.sync_all().err();
    }
    drop(f);
    if let Some(e) = failed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // `rename` 可能被杀毒软件 / 备份/同步工具短暂占住目标文件而失败：它们打开文件时
    // 不带 FILE_SHARE_DELETE，而 MoveFileEx 需要目标文件的 DELETE 权限。这类占用通常
    // 只持续几毫秒，而「保存设置失败」是个用户很难自解释的错误 —— 所以做几次有界重试。
    // 重试仍失败就如实报错，**绝不退化成「直接覆盖写」**那条非原子路径。
    let mut attempts_left = 3u32;
    loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts_left -= 1;
                if attempts_left == 0 {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
    }
}

/// 把 ≤1.2.5 留在 config.json 里的明文 `last_url` 迁移成 DPAPI 密文 `last_url_enc`
/// （读取-修改-写回，只动这两个键，其它字段原样保留）。
/// 返回 true = 盘上已处理完（明文键已消失）；false = 加密不可用或文件异常，什么都没动。
fn migrate_plaintext_last_url(app: &AppHandle, url: &str) -> bool {
    let Some(hex) = secret::seal(url) else { return false };
    let path = config_path(app);
    let Ok(s) = std::fs::read_to_string(&path) else { return false };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&s) else { return false };
    let Some(obj) = root.as_object_mut() else { return false };
    obj.insert("last_url_enc".to_string(), serde_json::Value::String(hex));
    obj.remove("last_url");
    let Ok(text) = serde_json::to_string_pretty(&root) else { return false };
    write_atomic(&path, text.as_bytes()).is_ok()
}

pub fn save(app: &AppHandle, cfg: &Config) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| {
        i18n::fmt("err_cfg_dir", &[&dir.display().to_string(), &e.to_string()])
    })?;
    let path = config_path(app);
    let s = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    // 原子替换（L-6）：写坏一次就等于把用户的全部设置抹成默认值
    write_atomic(&path, s.as_bytes()).map_err(|e| {
        i18n::fmt("err_cfg_write", &[&path.display().to_string(), &e.to_string()])
    })?;
    // config.json 一旦存在，语言以其中的 language 字段为准，sidecar 完成使命
    let _ = std::fs::remove_file(ui_language_sidecar_path(app));
    Ok(())
}

// ---------- 最近加载地址记忆（last_url_enc） ----------
//
// 这条记忆只记录「最近一次成功交给内嵌 WebView 的 DSH 完整地址」——它可能带
// `?token=<base64url>` 会话令牌，而该令牌能换取 30 天有效期的签名 cookie，
// 等于用户的 DSH 会话身份。所以（安全审查 MEDIUM-2）：
//
// - **明文绝不落盘**：写之前一律先 secret::seal()（Windows DPAPI，CurrentUser 作用域），
//   文件里只留十六进制密文；加密失败就干脆不记，绝不回落明文（代价只是这次不记地址）；
// - 它是**运行时记忆**、不是用户设置：不走设置页那套整体写盘。save_config 收到的是前端
//   按字段拼出来的 Config（不含本字段），所以那里显式沿用磁盘上的密文，见 process.rs；
// - 读取侧每次使用前在 process.rs::remembered_url_for 重新校验形状与端口；被手改、
//   损坏、换 Windows 用户解不开的记录一律当「没有」。它最多影响「连接现有服务 / 页面
//   重开」时加载哪个本机地址，那个 webview 没有任何 capability（remote origin + 无权限），
//   不存在提权面。
//
// 为什么不干脆删掉这条记忆：next 频道的裸地址会返回 401，没有它的话「连接现有服务」
// 与「页面重开」两个场景都要用户重新认证一次。

/// 更新 config.json 里的 `last_url_enc` 字段（读取-修改-写回，只动这一个键），失败静默。
/// 传空串 = 清除记录；密文写不进去时保持原值，**任何情况下都不会回落到明文**。
pub fn set_last_url(app: &AppHandle, url: &str) {
    let sealed = if url.trim().is_empty() {
        String::new()
    } else {
        match secret::seal(url) {
            Some(h) => h,
            // 加密不可用/失败：不写盘（保留原有记录），保持「没有明文落盘」这条不变量
            None => return,
        }
    };
    let path = config_path(app);
    let Ok(s) = std::fs::read_to_string(&path) else { return };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&s) else { return };
    // 磁盘上的 JSON 若被手改成非对象（数组/数字/字符串），直接放弃而不是 panic
    let Some(obj) = root.as_object_mut() else { return };
    obj.insert("last_url_enc".to_string(), serde_json::Value::String(sealed));
    // ≤1.2.5 留下的明文键：顺手删掉（只删这一个键，其它字段原样保留）
    obj.remove("last_url");
    if let Ok(text) = serde_json::to_string_pretty(&root) {
        let _ = write_atomic(&path, text.as_bytes());
    }
}

/// 读取最近一次记录的 DSH 页面地址（解开 `last_url_enc` 的密文）。
/// 缺失/空/解不开（换了 Windows 用户或机器、被手改）一律返回空串，形状校验在 process.rs。
///
/// 升级窗口：文件里可能还留着 ≤1.2.5 的明文 `last_url`（load() 启动时会把它加密成
/// `last_url_enc` 并整文件覆盖）。这里保留一条**只读**兼容分支，保证迁移落地前的同一次
/// 运行内行为不变；任何写路径（load 的迁移、set_last_url、save_config 保存）都会让那个
/// 明文键消失，所以它不会长期存在。
pub fn last_url(app: &AppHandle) -> String {
    let path = config_path(app);
    let Ok(s) = std::fs::read_to_string(&path) else { return String::new() };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&s) else { return String::new() };
    if let Some(hex) = root["last_url_enc"].as_str() {
        if !hex.trim().is_empty() {
            return secret::unseal(hex).unwrap_or_default();
        }
    }
    // 兼容分支：明文键只在迁移尚未落地时出现（见函数注释），同样是只读、用完即弃。
    root["last_url"].as_str().unwrap_or("").to_string()
}

// ---------- 「单键」写入（运行时记忆 / 单一偏好） ----------
//
// 为什么单独做这条路、而不是让前端 save_config 带上它们：
// 这些字段对应的是「一个动作只该改一个键」的操作（记住 Node 版本告警已确认、
// 用快捷键切换工具栏模式……）。走读取-修改-写回就不会顺手覆盖其它字段——
// save_config 是整体结构体写盘，前端一旦漏字段就可能把用户的确认/设置抹掉。

/// 只改 config.json 里的一个字符串键（读取-修改-写回）。
/// 写盘仍然走 L-6 的原子替换（`write_atomic`），**绝不回落**成直接覆盖。
fn write_config_key(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir)
        .map_err(|e| i18n::fmt("err_cfg_dir", &[&dir.display().to_string(), &e.to_string()]))?;
    let path = config_path(app);
    // 在「当前生效配置」上改这一个键：首次运行 config.json 还不存在时用默认配置，
    // 不会写出一个只有单个键的半成品文件。
    let mut root = serde_json::to_value(load(app)).unwrap_or_default();
    if !root.is_object() {
        root = serde_json::to_value(Config::default()).unwrap_or_default();
    }
    root[key] = serde_json::Value::String(value.trim().to_string());
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    write_atomic(&path, text.as_bytes())
        .map_err(|e| i18n::fmt("err_cfg_write", &[&path.display().to_string(), &e.to_string()]))
}

/// 记下「用户已知 Node 低于 <min_version>，仍选择继续」；传空串 = 取消这个选择
/// （首选项里取消勾选时走这里），恢复「版本过低就拦截启动」。
/// 写入的是**当时的下限**：以后程序把下限提高了，比对不相等就会重新告警。
pub fn remember_node_min_ack(app: &AppHandle, min_version: &str) -> Result<(), String> {
    write_config_key(app, "node_min_ack", min_version)
}

/// 记下工具栏模式偏好（调用方先用 `normalize_toolbar_mode` 归一化）。
/// 由快捷键 Ctrl+Shift+H 直接切换模式时调用：那条路径没有设置页表单可提交，
/// 只该动这一个键（首选项「保存」那条路仍走 save_config 整体提交）。
pub fn set_toolbar_mode(app: &AppHandle, mode: &str) -> Result<(), String> {
    write_config_key(app, "toolbar_mode", mode)
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
    write_atomic(&ui_language_sidecar_path(app), value.as_bytes()).map_err(|e| e.to_string())
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

// ---------- 原子写盘的离线回归（安全审查 L-6） ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    /// 下面有两个用例要「预测 `TMP_SEQ` 的下一个值」来制造临时名碰撞，而
    /// `cargo test` 默认多线程跑同一个二进制里的用例 —— 并行会让预测失效。
    /// 用一个全局锁把它们串起来（锁被 panic 毒化时直接复用内部值，不让它再传染）。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bytes(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    /// 独占创建一个测试用临时目录。**不用 `create_dir_all`**：名字撞上就直接失败，
    /// 不顺着已有目录往下写（与 `write_atomic` 用 `create_new` 是同一个思路）。
    fn scratch_dir(tag: &str) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dsh-config-test-{}-{}-{}",
            tag,
            std::process::id(),
            seq
        ));
        std::fs::create_dir(&dir).expect("创建测试临时目录失败");
        dir
    }

    #[test]
    fn tmp_path_for_stays_in_target_dir() {
        let _g = lock();
        let target = Path::new(r"C:\Users\me\AppData\Roaming\com.dsh.desktop\config.json");
        let tmp = tmp_path_for(target, 4321, 7);

        // 必须与目标**同目录**：`rename` 只在同卷内原子替换，丢到 %TEMP% 就白做了
        assert_eq!(tmp.parent(), target.parent());
        assert_eq!(
            tmp.file_name().unwrap().to_string_lossy().as_ref(),
            "config.json.tmp-4321-7"
        );
        // 临时名 ≠ 目标名 —— 否则「先写临时文件」就退化成「直接覆盖目标」了
        assert_ne!(tmp, target.to_path_buf());
    }

    #[test]
    fn write_atomic_creates_and_truncates() {
        let _g = lock();
        let dir = scratch_dir("replace");
        let target = dir.join("config.json");

        write_atomic(&target, &bytes(r#"{"port":3080}"#)).expect("首次写入");
        assert_eq!(std::fs::read(&target).unwrap(), bytes(r#"{"port":3080}"#));

        // 关键：第二次内容**更短**，必须把旧内容截断干净。
        // `fs::write` 也能截断，但它是「打开 + 截断 + 写入」三步，崩在中间就是半截
        // JSON（加载侧 `unwrap_or_default()` 会静默回落默认值 = 设置被抹掉）。
        // 这里用「内容恰好相等」顺带挡住「追加写」这类退步。
        write_atomic(&target, &bytes("{}")).expect("覆盖写入");
        assert_eq!(std::fs::read(&target).unwrap(), bytes("{}"));

        // 目录里只该剩下目标文件：临时文件必须已经被 rename 走（失败时则被清理掉）
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["config.json".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_refuses_preexisting_temp_name() {
        let _g = lock();
        let dir = scratch_dir("create_new");
        let target = dir.join("settings.yaml");

        // 预置一个「与即将使用的临时名完全相同」的文件：`write_atomic` 必须以
        // `create_new` 失败收场，而不是顺着这个已存在的文件（可能是别人放的符号链接）
        // 把内容写到别处去。
        let seq = TMP_SEQ.load(Ordering::Relaxed);
        let colliding = tmp_path_for(&target, std::process::id(), seq);
        std::fs::write(&colliding, b"attacker").unwrap();

        assert!(
            write_atomic(&target, &bytes("port: 3080\n")).is_err(),
            "临时名已存在时必须失败"
        );
        // 预置文件一个字节都没被动过，目标文件也不该被建出来
        assert_eq!(std::fs::read(&colliding).unwrap(), bytes("attacker"));
        assert!(!target.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 工具栏模式（pinned / auto） ----------

    /// 归一化：非法值 / 空值一律回落 pinned（历史行为），只认 auto 一族的写法。
    #[test]
    fn toolbar_mode_normalization() {
        assert_eq!(normalize_toolbar_mode("auto"), "auto");
        assert_eq!(normalize_toolbar_mode("  AUTO  "), "auto");
        assert_eq!(normalize_toolbar_mode("auto-hide"), "auto");
        assert_eq!(normalize_toolbar_mode("auto_hide"), "auto");
        assert_eq!(normalize_toolbar_mode("pinned"), "pinned");
        assert_eq!(normalize_toolbar_mode(""), "pinned");
        assert_eq!(normalize_toolbar_mode("floating"), "pinned");
        assert_eq!(Config::default().toolbar_mode, "pinned");
    }

    /// 老配置（没有 toolbar_mode 字段）必须反序列化成 pinned：这是「升级不改变行为」的
    /// 硬要求 —— 哪天有人把默认值改成 auto，所有人的工具栏都会在升级后突然消失。
    #[test]
    fn legacy_config_without_toolbar_mode_falls_back_to_pinned() {
        let legacy: Config =
            serde_json::from_str(r#"{"port":3080,"language":"zh"}"#).expect("老配置应能解析");
        assert_eq!(legacy.toolbar_mode, "pinned");
        assert_eq!(legacy.port, 3080);
        // 显式写了 auto 时照常生效（键名拼错 / 换写法由 normalize_toolbar_mode 兜底）
        let current: Config =
            serde_json::from_str(r#"{"toolbar_mode":"auto"}"#).expect("新配置应能解析");
        assert_eq!(current.toolbar_mode, "auto");
    }
}
