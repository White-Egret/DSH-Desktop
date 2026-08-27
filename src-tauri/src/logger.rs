//! 文件日志：
//! - Desktop 自身日志：%APPDATA%\com.dsh.desktop\desktop.log
//!   （launcher/update/setup 以及 DSH stdout/stderr 的镜像都写这里）
//! - DSH 输出日志：<DSH 家目录>\logs\dsh.log（默认即 %USERPROFILE%\.dsh\logs\dsh.log）
//!
//! 全部为追加写入，超过 5MB 时滚动为 *.old；任何写入失败都静默忽略，
//! 绝不影响主流程（日志只是辅助设施）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static LOG_LOCK: Mutex<()> = Mutex::new(());

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Desktop 日志路径：%APPDATA%\<identifier>\desktop.log
pub fn desktop_log_path(app: &tauri::AppHandle) -> PathBuf {
    crate::config::config_dir(app).join("desktop.log")
}

/// DSH 输出日志路径：<DSH 家目录>\logs\dsh.log
pub fn dsh_log_path(dsh_home_dir: &str) -> PathBuf {
    PathBuf::from(dsh_home_dir).join("logs").join("dsh.log")
}

/// UTC 时间戳（无第三方依赖的精简实现）
pub fn utc_now_string() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    )
}

/// Howard Hinnant 的 days→civil 算法（公有领域）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 追加一行到日志文件（自动建目录、超限滚动、失败忽略）
pub fn append_line(path: &Path, line: &str) {
    let _guard = LOG_LOCK.lock().unwrap();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_LOG_BYTES {
            let mut old = path.as_os_str().to_os_string();
            old.push(".old");
            let _ = std::fs::rename(path, PathBuf::from(old));
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let clean = line.replace('\r', "");
        let _ = writeln!(f, "[{}] {}", utc_now_string(), clean);
    }
}
