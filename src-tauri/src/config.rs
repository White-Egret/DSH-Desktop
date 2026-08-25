use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// Launcher 的持久化配置，保存于 %APPDATA%\<identifier>\config.json
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// npm.cmd 完整路径（用于更新 DSH / 查询最新版本）
    pub npm_path: String,
    /// dsh.cmd 完整路径（用于启动 DSH）
    pub dsh_path: String,
    /// DSH 家目录：DSH 摆放配置文件的地方（通过 DSH_HOME 环境变量传给 DSH）
    /// 启动进程的工作目录自动取其上一级目录；DSH 的工作区在网页内随意指定
    pub dsh_home_dir: String,
    /// DSH Web 服务端口
    pub port: u16,
    /// 附加启动参数（空格分隔，追加在 `dsh web --port <port> --no-open` 之后）
    pub extra_args: String,
    /// DSH 的 npm 包名（用于 `npm view <name> version` 与提示）
    pub package_name: String,
    /// 更新参数（拼在 `cmd /C <npm_path>` 之后）
    pub update_args: String,
    /// 等待 DSH 就绪的超时时间（秒）；0 = 一直等待（只要 DSH 进程还活着）
    pub health_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            npm_path: r"D:\Programs\nodejs\npm.cmd".to_string(),
            dsh_path: r"C:\Users\admin\AppData\Roaming\npm\dsh.cmd".to_string(),
            dsh_home_dir: default_dsh_home_dir(),
            port: 3080,
            extra_args: String::new(),
            package_name: "@deepseek-ai/dsh".to_string(),
            update_args: "install -g @deepseek-ai/dsh@latest".to_string(),
            // DSH 冷启动（尤其重启电脑后首次）可能需要 1~2 分钟以上，默认给足 5 分钟
            health_timeout_secs: 300,
        }
    }
}

fn default_dsh_home_dir() -> String {
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| r"C:\Users\admin".to_string());
    PathBuf::from(base)
        .join(".dsh")
        .to_string_lossy()
        .to_string()
}

/// DSH 进程的工作目录：家目录的上一级（如 C:\Users\admin\.dsh -> C:\Users\admin）
pub fn workspace_cwd(cfg: &Config) -> String {
    PathBuf::from(&cfg.dsh_home_dir)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\admin".to_string())
        })
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
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&s) {
            return cfg;
        }
    }
    // 自动迁移旧目录 com.dsh.launcher → com.dsh.desktop，避免升级后配置丢失
    if let Some(old) = legacy_config_path() {
        if old != path {
            if let Ok(s) = std::fs::read_to_string(&old) {
                if let Ok(cfg) = serde_json::from_str::<Config>(&s) {
                    let _ = save(app, &cfg);
                    return cfg;
                }
            }
        }
    }
    Config::default()
}

pub fn save(app: &AppHandle, cfg: &Config) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("无法创建配置目录 {}: {}", dir.display(), e))?;
    let path = config_path(app);
    let s = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, s)
        .map_err(|e| format!("无法写入配置文件 {}: {}", path.display(), e))?;
    Ok(())
}
