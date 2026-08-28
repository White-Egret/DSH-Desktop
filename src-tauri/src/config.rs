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
    /// DSH 的 npm 包名（用于 `npm view <name> version` 与提示）
    pub package_name: String,
    /// 更新参数（拼在 `cmd /C <npm_path>` 之后）
    pub update_args: String,
    /// 等待 DSH 就绪的超时时间（秒）；0 = 一直等待（只要 DSH 进程还活着）
    pub health_timeout_secs: u64,
    /// 点击主窗口 X 时的行为："tray" = 隐藏到托盘（默认），"quit" = 退出程序
    pub close_action: String,
    /// 界面语言："zh" = 中文（默认），"en" = English。
    /// 保存时同步写入 DSH 家目录 settings.yaml 的 locale.preference，
    /// 让 DSH 自身的 Web 界面跟随中英文切换。
    pub language: String,
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
            update_args: "install -g @deepseek-ai/dsh@latest".to_string(),
            // DSH 冷启动（尤其重启电脑后首次）可能需要 1~2 分钟以上，默认给足 5 分钟
            health_timeout_secs: 300,
            // 默认点 X 隐藏到托盘（后台继续运行）；可在设置页改为退出程序
            close_action: "tray".to_string(),
            // 默认中文界面
            language: "zh".to_string(),
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

/// DSH 进程的工作目录：家目录的上一级（如 C:\Users\<你>\.dsh -> C:\Users\<你>）
pub fn workspace_cwd(cfg: &Config) -> String {
    PathBuf::from(&cfg.dsh_home_dir)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string())
        })
}

/// 把界面语言写入 `<DSH 家目录>\settings.yaml` 的 locale.preference（最小侵入式行编辑）。
/// - 已有 `locale:` 块与 `preference:` 行 → 仅替换该行；
/// - 有 `locale:` 块但没有 preference → 在块首插入；
/// - 完全没有 → 文件末尾追加 `locale:\n  preference: <zh|en>` 块；
/// - 文件不存在 → 创建仅含该块的新文件。
/// 其余行原样保留，不引入 YAML 解析依赖。
pub fn sync_dsh_locale(home_dir: &str, language: &str) -> Result<(), String> {
    let pref = if language.eq_ignore_ascii_case("en") { "en" } else { "zh" };
    let dir = PathBuf::from(home_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    let path = dir.join("settings.yaml");
    let content = if path.is_file() {
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?
    } else {
        String::new()
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    let mut found_locale = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        // 顶层 locale: 键（行首无缩进、去掉尾部空白后恰为 "locale:"）
        if line.trim_end() == "locale:" && !line.starts_with(' ') && !line.starts_with('\t') {
            found_locale = true;
            out.push(line.to_string());
            i += 1;
            // 收集该键的缩进子块（含空行/注释），并在其中替换/插入 preference
            let mut block: Vec<String> = Vec::new();
            let mut replaced = false;
            while i < lines.len() {
                let bl = lines[i];
                if bl.trim().is_empty() {
                    block.push(bl.to_string());
                    i += 1;
                    continue;
                }
                if !bl.starts_with(' ') && !bl.starts_with('\t') {
                    break; // 到达下一个顶层键
                }
                if bl.trim_start().starts_with("preference:") {
                    block.push(format!("  preference: {}", pref));
                    replaced = true;
                } else {
                    block.push(bl.to_string());
                }
                i += 1;
            }
            if !replaced {
                block.insert(0, format!("  preference: {}", pref));
            }
            out.extend(block);
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    if !found_locale {
        if !out.is_empty() && !out.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            out.push(String::new());
        }
        out.push("locale:".to_string());
        out.push(format!("  preference: {}", pref));
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {}", path.display(), e))?;
    Ok(())
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
    let mut cfg = if let Ok(s) = std::fs::read_to_string(&path) {
        serde_json::from_str::<Config>(&s).unwrap_or_default()
    } else {
        // 自动迁移旧目录 com.dsh.launcher → com.dsh.desktop，避免升级后配置丢失
        first_run = true;
        let mut migrated = None;
        if let Some(old) = legacy_config_path() {
            if old != path {
                if let Ok(s) = std::fs::read_to_string(&old) {
                    if let Ok(c) = serde_json::from_str::<Config>(&s) {
                        let _ = save(app, &c);
                        migrated = Some(c);
                    }
                }
            }
        }
        migrated.unwrap_or_default()
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
