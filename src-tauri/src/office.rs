// DSH Desktop —— OnlyOffice 能力桥接：状态探测 / 一键安装 / 卸载（P3）
//
// 模块结构照 `py_bridge.rs`，安装流水线照方案 §3.3，卸载照 §6。三处硬边界：
//   ① 对 Document Server 的**服务操作**只发生在提权脚本里（elevate::run_elevated），
//      本模块在普通权限下只做「读注册表 / 读文件 / 发 HTTP 探测」；
//   ② 提权脚本一律走 [`crate::elevate::run_elevated`]（脚本 UTF-8 带 BOM 落盘 +
//      结果文件轮询）——P0 实测（§10.2/§10.3）：PS 5.1 把无 BOM UTF-8 按 GBK 读，
//      从后台线程直接发起 ShellExecute 偶发卡死；
//   ③ 下载只认 §10.1 认证过的那个 URL + 钉死的 SHA-256，哈希不符直接中止，
//      不提供「仍然安装」。
//
// 与 py-bridge 的结构差异：office-bridge **没有**独立的运行时目录（插件本身
// 就是运行时），所以只有 bundle 一个释放目录；phase 状态机的输入因此少一个布尔。

use std::process::Stdio;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::config;
use crate::i18n;
use crate::logger;
use crate::process::{self, AppState, LogEvent, SetupResult};

// ---------- 模块级常量（照 py_bridge.rs:44-56 的组织方式） ----------

/// 释放进 `<应用配置目录>\office-bridge-bundle\` 的文件（与
/// `resources/office-bridge-bundle/` 一一对应；比 py-bridge 多一份 client.js）。
/// 少一条 include_str! 就是插件在 profile 里缺文件、boot 期直接 failed。
const OFFICE_BUNDLE_FILES: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../resources/office-bridge-bundle/package.json"),
    ),
    (
        "cordis.patch.yml",
        include_str!("../resources/office-bridge-bundle/cordis.patch.yml"),
    ),
    (
        "index.js",
        include_str!("../resources/office-bridge-bundle/index.js"),
    ),
    (
        "client.js",
        include_str!("../resources/office-bridge-bundle/client.js"),
    ),
];

/// bundle 目录名（`<应用配置目录>\office-bridge-bundle`）
const BUNDLE_DIR_NAME: &str = "office-bridge-bundle";
/// 宿主插件在 profile 里的包名（与 bundle 的 package.json `name` 一致）
const BUNDLE_PKG_NAME: &str = "@local/dsh-desktop-office-bridge";
/// config.json 里的 pending 标记键（office_bridge_pending **同时**是 Config 正式字段，
/// 见 config.rs 该字段的注释 —— 双保险，缺一不可）
const PENDING_KEY: &str = "office_bridge_pending";

// ---------- 钉死的认证值（§10.1；2026-11-04 在本机对演练资产复核通过） ----------

/// 官方下载地址（HTTP 200 直达、无重定向； community 版全量包）
const OFFICE_DOWNLOAD_URL: &str =
    "https://download.onlyoffice.com/install/documentserver/windows/onlyoffice-documentserver.exe";
/// 认证过的 SHA-256（小写）。上游换包（哪怕同版本重发布）都会在这里被拦下 ——
/// 这是**刻意**的：换包必须重新走一遍 P0 的人工认证，不能静默信任新字节。
const OFFICE_INSTALLER_SHA256: &str =
    "5106656fe128828f17d636bd784c2a24ae1be5d95563635df7fcce1e531d1cc5";
/// 认证过的文件大小（字节）。先于哈希生效：一个 quick check 就能发现「下错文件」。
const OFFICE_INSTALLER_BYTES: u64 = 916_210_008;
/// Authenticode 签发者主体（Get-AuthenticodeSignature 的 SignerCertificate.Subject
/// 以这个子串开头才算 Valid 且来自官方）
const OFFICE_SIGNER_CN: &str = "CN=Ascensio System SIA";
/// 手动下载页（失败文案里给用户的退路）
const OFFICE_MANUAL_PAGE: &str =
    "https://helpcenter.onlyoffice.com/docs/installation/docs-community-install-windows.aspx";

/// 四个 Windows 服务的实际名字（P0 实测：helpcenter 文档写的 `ds-docservice` 系
/// **不存在**；重启用 DisplayName `*ONLYOFFICE*` 通配，见 config-merge 脚本）
const OFFICE_SERVICES: [&str; 4] = [
    "DsDocServiceSvc",
    "DsConverterSvc",
    "DsProxySvc",
    "DsExampleSvc",
];

/// Document Server 默认端口（与 config.rs 的 Default 同值；那里管持久化，这里管兜底）
const DEFAULT_DOCSERVER_PORT: u16 = 8080;
const DEFAULT_DOC_PORT: u16 = 3310;
/// 安装健康等待上限（秒）：服务重启 + nginx 起监听，P0 实测量级在 1 分钟内
const HEALTH_WAIT_SECS: u64 = 180;
/// Inno Setup 卸载键（DisplayVersion / InstallLocation / UninstallString 都在这；
/// 本机实测 `DisplayVersion=10.0.0.169`。§3.1 原设想「读 default.json 的 version
/// 字段」不成立——那个 `version: 3` 是配置 schema 版本，不是产品版本）。
/// ⚠ 必须带 `HKLM\` 蜂巢前缀：`reg.exe query` 要的是完整路径，只写
/// `SOFTWARE\...` 会报「找不到」（2026-11-04 真机验证抓出），探测会永远
/// 退化成「未安装」。
const OFFICE_UNINSTALL_KEY: &str =
    r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\ONLYOFFICE DocumentServer_is1";

// ---------- bundle 目录与释放 ----------

/// `<应用配置目录>\office-bridge-bundle`
pub fn bundle_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(BUNDLE_DIR_NAME)
}

/// bundle 是否已完整释放（四个文件都在；空壳目录不算 —— 与 py_bridge 同一口径）
fn bundle_released(bun: &Path) -> bool {
    OFFICE_BUNDLE_FILES.iter().all(|(name, _)| bun.join(name).is_file())
}

/// 幂等释放：临时文件 + 原子改名覆盖（照 py_bridge::write_files，:685-715）。
/// 重复点击安装不会留下半份文件；rename 失败先删临时文件再报错。
fn write_files(dir: &Path, files: &[(&str, &str)]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        i18n::fmt("office_write_fail", &[&dir.to_string_lossy(), &e.to_string()])
    })?;
    for (name, body) in files {
        let target = dir.join(name);
        // 临时名带进程内序号：同名文件不会被自己覆盖成空
        let tmp = dir.join(format!("{}.{}.tmp", name, std::process::id()));
        std::fs::write(&tmp, body).map_err(|e| {
            i18n::fmt("office_write_fail", &[&tmp.to_string_lossy(), &e.to_string()])
        })?;
        std::fs::rename(&tmp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            i18n::fmt("office_write_fail", &[&target.to_string_lossy(), &e.to_string()])
        })?;
    }
    Ok(())
}

/// 幂等释放 bundle 四件套。
fn release_bundle_sources(bun: &Path) -> Result<(), String> {
    write_files(bun, OFFICE_BUNDLE_FILES)
}

/// 删释放目录；「被占用」（os error 32/5，如 DSH 还开着、插件目录被 pnpm 占住）
/// 单独归类 —— 文件其实已删光，空壳会在下次进入 DSH 时由
/// [`cleanup_stale_office_dirs`] 清掉（照 py_bridge::remove_bridge_dir :445-457）。
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

/// 进门清理空壳目录（set_status 在 DSH 起来后调用；此刻插件进程还没 spawn，
/// 目录不再被锁）。返回 Some = 清掉了，值得一行日志。
pub fn cleanup_stale_office_dirs(app: &AppHandle) -> Option<String> {
    let dir = bundle_dir(&config::config_dir(app));
    if !dir.exists() {
        return None;
    }
    // 非空 → 有效的插件代码，不动
    match std::fs::read_dir(&dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return None;
            }
        }
        Err(_) => return None,
    }
    if std::fs::remove_dir(&dir).is_ok() {
        let line = i18n::fmt("office_stale_dir_removed", &[&dir.to_string_lossy()]);
        logger::append_line(&logger::desktop_log_path(app), &line);
        return Some(line);
    }
    None
}

// ---------- 插件 phase 状态机（照 py_bridge.rs:150-192） ----------

/// 待激活状态机：由「bundle 释放了吗 / bundle 装进 profile 了吗」两个事实推出
/// 界面上该显示什么。py-bridge 的第三个输入（Python 运行时在不在）在这里不存在
/// —— 插件本身就是运行时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeBridgePhase {
    /// 未部署（没有释放、profile 里也没有）
    NotInstalled,
    /// profile 里有注册残留、但释放目录没了（activate_pending 会自动修复）
    NeedsRelease,
    /// 已释放、还没写进 profile（首次安装 / DSH 在跑时的 pending）
    PendingActivate,
    /// bundle 已在 profile 里
    Active,
}

impl OfficeBridgePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NeedsRelease => "needs-release",
            Self::PendingActivate => "pending-activate",
            Self::Active => "active",
        }
    }
}

/// 纯函数：状态机必须能单测，全部输入就是这两个布尔。
/// pending 标记不参与判定 —— 它意味着「注册还没做」（与 py_bridge 同一纪律），
/// 只影响文案与降级（profile 没探测出来时退回「待激活」而不是误报「未部署」）。
pub fn office_bridge_phase(released: bool, registered: bool) -> OfficeBridgePhase {
    match (released, registered) {
        (false, false) => OfficeBridgePhase::NotInstalled,
        (false, true) => OfficeBridgePhase::NeedsRelease,
        (true, false) => OfficeBridgePhase::PendingActivate,
        (true, true) => OfficeBridgePhase::Active,
    }
}

// ---------- 状态探测（office_status 命令，§3.1） ----------

/// 首选项「OnlyOffice」块的状态（只读）。前端显示 `ds_message` / `plugin_message`
/// 两行（§2.2），文案由后端按当前语言生成，前端不自己拼。
#[derive(Clone, Serialize)]
pub struct OfficeStatus {
    /// Document Server 是否已安装（注册表卸载键，兜底 config 记录的目录）
    pub installed: bool,
    /// 产品版本（注册表 DisplayVersion，如 "10.0.0.169"；拿不到为空串）
    pub version: String,
    /// 实际安装目录（注册表 InstallLocation；兜底 config 的 office_install_dir）
    pub install_dir: String,
    /// 卸载器路径（注册表 UninstallString 解出来的 exe；卸载命令用它）
    pub uninstaller: String,
    /// Document Server 端口（config，默认 8080）
    pub docserver_port: u16,
    /// 插件文档服务端口（config，默认 3310）
    pub doc_port: u16,
    /// 四个服务里处于 Running 的个数
    pub services_running: u32,
    pub services_total: u32,
    /// `GET /healthcheck` 返回 true
    pub healthcheck_ok: bool,
    /// 插件 phase（OfficeBridgePhase 的字符串形态）
    pub phase: String,
    pub released: bool,
    pub registered: bool,
    pub pending: bool,
    /// 实际使用的 profile（没探测到为空串）
    pub profile: String,
    /// bundle 释放目录
    pub bundle_dir: String,
    /// 状态行第一行：Document Server 的事实
    pub ds_message: String,
    /// 状态行第二行：AI 桥接插件的事实
    pub plugin_message: String,
    /// 两行合并（日志用）
    pub message: String,
}

#[tauri::command]
pub async fn office_status(app: AppHandle) -> Result<OfficeStatus, String> {
    // 探测要起子进程（reg.exe / powershell），下到阻塞线程池（照 python_status 的纪律）
    tauri::async_runtime::spawn_blocking(move || office_status_blocking(&app))
        .await
        .map_err(|e| e.to_string())
}

/// 安装探测结果（office_status 与安装/卸载流程共用）。
#[derive(Debug, Clone, Default)]
pub struct InstallProbe {
    pub installed: bool,
    pub version: String,
    pub install_dir: String,
    pub uninstaller: String,
}

fn office_status_blocking(app: &AppHandle) -> OfficeStatus {
    let cfg = config::load(app);
    let cfg_dir = config::config_dir(app);
    let bun = bundle_dir(&cfg_dir);

    let probe = probe_install(app);
    let services_running = probe_services();
    let services_total = OFFICE_SERVICES.len() as u32;
    // 服务没起时 healthcheck 必然连不上：先探服务、不通就跳过 HTTP（省一次超时等待）
    let healthcheck_ok = if probe.installed && services_running > 0 {
        healthcheck_ok(cfg.office_docserver_port)
    } else {
        false
    };

    let released = bundle_released(&bun);
    let profile = py_bridge_probe_profile(app);
    let registered = profile.as_deref().map(py_bridge_bundle_registered).unwrap_or(false);
    let pending = config::read_config_flag(app, PENDING_KEY);
    // pending 只作降级依据（profile 没探测出来时退回「待激活」），不参与 phase 判定
    let effective_registered = registered || (pending && profile.is_none());
    let phase = office_bridge_phase(released, effective_registered);

    let ds_message = build_ds_message(&probe, docserver_port_of(&cfg), services_running, services_total, healthcheck_ok);
    // DS 不在时，插件行的「注册残留/待激活」话术只会让人困惑（用户刚卸载完，
    // 看到的是「会自动修复」却什么都没发生）：统一口径为「未部署」。
    let plugin_message = if !probe.installed {
        i18n::t("office_bridge_missing").to_string()
    } else {
        bridge_phase_message(phase, pending, profile.as_deref())
    };

    OfficeStatus {
        installed: probe.installed,
        version: probe.version.clone(),
        install_dir: probe.install_dir.clone(),
        uninstaller: probe.uninstaller.clone(),
        docserver_port: docserver_port_of(&cfg),
        doc_port: if cfg.office_doc_port > 0 { cfg.office_doc_port } else { DEFAULT_DOC_PORT },
        services_running,
        services_total,
        healthcheck_ok,
        phase: phase.as_str().to_string(),
        released,
        registered,
        pending,
        profile: profile.unwrap_or_default(),
        bundle_dir: bun.to_string_lossy().to_string(),
        ds_message: ds_message.clone(),
        plugin_message: plugin_message.clone(),
        message: format!("{ds_message}\n{plugin_message}"),
    }
}

fn docserver_port_of(cfg: &config::Config) -> u16 {
    if cfg.office_docserver_port > 0 { cfg.office_docserver_port } else { DEFAULT_DOCSERVER_PORT }
}

fn build_ds_message(
    probe: &InstallProbe,
    port: u16,
    services_running: u32,
    services_total: u32,
    healthcheck_ok: bool,
) -> String {
    if !probe.installed {
        return i18n::t("office_ds_missing").to_string();
    }
    let ver = if probe.version.is_empty() {
        i18n::t("office_ver_unknown").to_string()
    } else {
        probe.version.clone()
    };
    let svc = if services_running == 0 {
        i18n::t("office_svc_none").to_string()
    } else if services_running == services_total {
        i18n::t("office_svc_running").to_string()
    } else {
        i18n::fmt("office_svc_partial", &[&services_running.to_string(), &services_total.to_string()])
    };
    let health = if healthcheck_ok {
        i18n::t("office_health_ok").to_string()
    } else {
        i18n::t("office_health_fail").to_string()
    };
    i18n::fmt(
        "office_ds_ok",
        &[&ver, &format!("127.0.0.1:{port}"), &svc, &health],
    )
}

fn bridge_phase_message(phase: OfficeBridgePhase, pending: bool, profile: Option<&str>) -> String {
    match phase {
        OfficeBridgePhase::NotInstalled => i18n::t("office_bridge_missing").to_string(),
        OfficeBridgePhase::NeedsRelease => i18n::t("office_bridge_needs_release").to_string(),
        OfficeBridgePhase::PendingActivate => i18n::t("office_bridge_pending").to_string(),
        OfficeBridgePhase::Active => {
            if pending {
                // 注册已发生但标记还在（或 DSH 在跑时注册被延后）：按 py_bridge 的口径
                // 显示「已注册进 profile」，不谎称「已激活」
                i18n::fmt("office_bridge_registered_pending", &[&profile.unwrap_or("")])
            } else {
                i18n::fmt("office_bridge_active", &[&profile.unwrap_or("")])
            }
        }
    }
}

// ---------- py_bridge 复用层的薄包装（跨模块调用收拢到一处，方便审计） ----------

fn py_bridge_probe_profile(app: &AppHandle) -> Option<String> {
    crate::py_bridge::probe_profile(app)
}

fn py_bridge_bundle_registered(profile: &str) -> bool {
    crate::py_bridge::bundle_registered(profile)
}

// ---------- Document Server 安装探测 ----------

/// 探测 Document Server：**注册表卸载键是唯一权威判据**。
/// 任何一步失败都退化成「未安装」，绝不报错（状态行不是报错入口）。
///
/// ⚠ 刻意**不做**「config 记录的目录兜底」：卸载后官方卸载器会留 ~45MB 残余目录，
/// 那个目录存在、注册表键已删 —— 兜底会让 `office_status` 在卸载后仍然报「已安装」，
/// 「安装 OnlyOffice」按钮也随之消失（真机 2026-11-06 卸载实测）。残余目录的
/// 清理走「清扫残余」按钮（它读的是卸载时记录下的 `office_install_dir`），
/// 不该反过来污染安装探测。
fn probe_install(_app: &AppHandle) -> InstallProbe {
    probe_install_registry()
}

fn probe_install_registry() -> InstallProbe {
    let mut probe = InstallProbe::default();
    // reg.exe 直接读参数、不经 cmd.exe（与 detect.rs 同款纪律）；/reg:64 钉死 64 位视图
    let (ok, out) = match process::run_cmd_capture(
        "reg.exe",
        &["query".to_string(), OFFICE_UNINSTALL_KEY.to_string(), "/reg:64".to_string()],
        "",
        Duration::from_secs(15),
    ) {
        Ok(r) => r,
        Err(_) => return probe,
    };
    if !ok {
        // 键不存在 = 未安装（正常路径，不是错误）
        return probe;
    }
    let (version, location, uninstaller) = parse_uninstall_reg(&out);
    probe.version = version;
    probe.install_dir = location.clone();
    probe.uninstaller = uninstaller;
    // 目录真实存在才算安装（键残留、目录被手删的机器不算）
    probe.installed = !location.is_empty() && Path::new(&location).is_dir();
    probe
}

/// 从 `reg query <key>` 的输出里解析三个值（纯函数，单测覆盖）。
///
/// 输出行形如 `    DisplayVersion    REG_SZ    10.0.0.169`，本地化只影响
/// `REG_SZ` 两侧的空白宽度，键名与值本身不受影响。值两侧的引号（Inno 的
/// UninstallString 是 `"D:\...\unins001.exe"`）在这里剥掉 —— 调用方拿到的
/// 必须是能直接 `Path::new(..).is_file()` 的裸路径。
fn parse_uninstall_reg(out: &str) -> (String, String, String) {
    let mut version = String::new();
    let mut location = String::new();
    let mut uninstaller = String::new();
    for line in out.lines() {
        let Some(idx) = line.find("REG_SZ") else { continue };
        let name = line[..idx].trim().trim_end();
        // reg 的对齐空白可能把键名与 REG_SZ 隔开；取「REG_SZ 之前」的最后一段
        // ⚠ name 取「REG_SZ 之前」的最后一个空白分隔 token；str 没有
        // rsplit_whitespace（CI 编译抓出），用 split_whitespace().last() 等价实现
        let name = name.split_whitespace().last().unwrap_or("");
        let value = line[idx + "REG_SZ".len()..].trim();
        let value = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(value)
            .to_string();
        match name {
            "DisplayVersion" => version = value,
            "InstallLocation" => location = value.trim_end_matches('\\').to_string(),
            "UninstallString" => uninstaller = value,
            _ => {}
        }
    }
    (version, location, uninstaller)
}

/// 四个服务里几个在跑。PowerShell 的 `-eq 'Running'` 是**枚举名比较**，
/// 不受系统显示语言影响（zh-CN 上 sc.exe 的输出反而可能本地化）。
fn probe_services() -> u32 {
    let script = format!(
        "(Get-Service -Name '{}' -ErrorAction SilentlyContinue | ForEach-Object {{ if ($_.Status -eq 'Running') {{'1'}} else {{'0'}} }}) -join ''",
        OFFICE_SERVICES.join("','")
    );
    let out = match run_powershell(&script, Duration::from_secs(20)) {
        Ok(o) => o,
        Err(_) => return 0,
    };
    out.chars().filter(|c| *c == '1').count() as u32
}

/// `GET http://127.0.0.1:<port>/healthcheck`，官方验收标准是响应体 `true`。
/// 手写 HTTP/1.0（与 process::http_once 同款 TcpStream 风格，零新依赖），
/// 但这里要读完整 body，所以读到对端关连接为止。
fn healthcheck_ok(port: u16) -> bool {
    healthcheck_body(port).map(|b| b.contains("true")).unwrap_or(false)
}

fn healthcheck_body(port: u16) -> Option<String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!(
        "GET /healthcheck HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: DSH-Desktop/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if raw.len() > 1024 * 1024 {
            break; // 健康检查不可能这么大：当作异常，绝不无限读
        }
    }
    let text = String::from_utf8_lossy(&raw);
    // 跳过头部（\r\n\r\n 之后才是 body）
    text.split("\r\n\r\n").nth(1).map(|b| b.to_string())
}

/// 端口预检的「实际试绑」（§10.3 坑 1：Windows 保留段 10013，netstat 看不出来，
/// 只查占用没用）。绑上立刻放行 —— 真正的服务由安装器/插件稍后去监听。
fn port_bindable(port: u16) -> bool {
    use std::net::TcpListener;
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => {
            drop(l);
            true
        }
        Err(_) => false,
    }
}

// ---------- PowerShell 小工具（全部 -EncodedCommand，M-4 纪律） ----------

/// 跑一段 PowerShell（Base64(UTF-16LE) 编码传入），返回 stdout+stderr 合并文本。
/// 统一关掉进度输出：Get-* 系列首次加载模块会往 stderr 写 CLIXML 进度记录，
/// 混进 run_cmd_capture 的合并输出里会干扰调用方的行解析（真机验证实录）。
fn run_powershell(script: &str, timeout: Duration) -> Result<String, String> {
    let full = format!("$ProgressPreference = 'SilentlyContinue'\n{script}");
    let enc = process::encode_powershell_command(&full);
    process::run_cmd_capture(
        "powershell.exe",
        &[
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-EncodedCommand".to_string(),
            enc,
        ],
        "",
        timeout,
    )
    .map(|(_, out)| out)
}

/// 大文件的 SHA-256：`Get-FileHash` 是流式的 —— 874 MB 的安装包不能整块读进
/// 内存（process::sha256_hex_of 会 `fs::read` 整个文件）。
fn sha256_of_large_file(path: &Path) -> Result<String, String> {
    let script = format!(
        "(Get-FileHash -LiteralPath '{}' -Algorithm SHA256).Hash",
        process::ps_single_quote(&path.to_string_lossy())
    );
    let out = run_powershell(&script, Duration::from_secs(15 * 60))?;
    let hex: String = out.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() < 64 {
        return Err(out.trim().to_string());
    }
    Ok(hex[..64].to_ascii_lowercase())
}

/// Authenticode 校验：`Valid` 且签发者主体以官方 CN 开头。
/// 返回 Err(人话原因) = 校验未通过。
fn verify_authenticode(path: &Path) -> Result<(), String> {
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{}'; if ($null -eq $s.SignerCertificate) {{ 'None|' }} else {{ \"$($s.Status)|$($s.SignerCertificate.Subject)\" }}",
        process::ps_single_quote(&path.to_string_lossy())
    );
    let out = run_powershell(&script, Duration::from_secs(60))?;
    let line = out.lines().rev().find(|l| l.contains('|')).unwrap_or("").trim();
    let (status, subject) = line.split_once('|').unwrap_or(("None", ""));
    if status != "Valid" {
        return Err(i18n::fmt("office_verify_signer", &[&format!("status={status}")]));
    }
    if !subject.starts_with(OFFICE_SIGNER_CN) {
        return Err(i18n::fmt("office_verify_signer", &[&subject]));
    }
    Ok(())
}

// ---------- local.json 合并（§3.3 ⑤；纯函数在 tests 里覆盖） ----------

/// 从 local.json 文本里读出现有 JWT 密钥（services.CoAuthoring.secret.inbox.string）。
/// 重装时优先复用旧密钥：插件配置与 DS 配置就不需要同步轮换。
fn existing_secret_of(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("services")?
        .get("CoAuthoring")?
        .get("secret")?
        .get("inbox")?
        .get("string")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// 生成新的 JWT 密钥（32 位小写十六进制，与 P0 演练的密钥形态一致）。
/// 熵源：时间纳秒 ^ 进程 id ^ 栈地址（ASLR）→ SHA-256 取前 32 位。
/// 本机 127.0.0.1 场景下密钥的威胁模型是「同用户进程可读 local.json」（§8），
/// 这个熵对同一用户的其他进程不可预测，够用。
fn new_office_secret() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let stack = &nanos as *const u64 as u64;
    let data = format!("{nanos:x}{pid:x}{stack:x}");
    process::sha256_hex(data.as_bytes())[..32].to_string()
}

/// 把 §3.3 ⑤ 列出的键合并进 local.json，**保留安装器写进去的其它键**
/// （license / runtimeConfig / wopi / storage … 实测都在里面）。
/// 纯函数：解析失败的 local.json 一律报错中止，绝不静默重写整份配置。
fn merge_local_json(existing: &str, secret: &str) -> Result<String, String> {
    let mut root: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing)
            .map_err(|e| i18n::fmt("office_local_json_bad", &[&e.to_string()]))?
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    // serde_json 对 Null 自动建对象，但对 String/Number 索引会 panic ——
    // local.json 是我们不完全掌控的文件（安装器写的），先归一化再动键。
    fn obj<'a>(v: &'a mut serde_json::Value, key: &str) -> &'a mut serde_json::Value {
        if !v.get(key).map(|x| x.is_object()).unwrap_or(false) {
            v[key] = serde_json::json!({});
        }
        &mut v[key]
    }
    {
        let rfa = obj(&mut root, "request-filtering-agent");
        rfa["allowPrivateIPAddress"] = serde_json::Value::Bool(true);
        rfa["allowMetaIPAddress"] = serde_json::Value::Bool(true);
    }
    {
        let co = obj(obj(&mut root, "services"), "CoAuthoring");
        let token = obj(co, "token");
        let enable = obj(token, "enable");
        let request = obj(enable, "request");
        request["inbox"] = serde_json::Value::Bool(true);
        request["outbox"] = serde_json::Value::Bool(true);
        enable["browser"] = serde_json::Value::Bool(true);
        let secret_v = obj(co, "secret");
        for k in ["inbox", "outbox", "session"] {
            obj(secret_v, k)["string"] = serde_json::Value::String(secret.to_string());
        }
    }
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

// ---------- bundle 配置按键替换（照 py_bridge::patch_bundle_config） ----------

/// 把实际值写进 bundle 的 cordis.patch.yml：逐行扫、按键替换、保留注释与键序。
/// 四个键的形态与 prepare-bundle 的输出一致：三个字符串带单引号，docPort 是裸数字。
fn patch_office_bundle_config(
    text: &str,
    docserver_url: &str,
    jwt_secret: &str,
    doc_port: u16,
    workspace_dir: &str,
) -> String {
    let quote = |s: &str| s.replace('\'', "''");
    let url = quote(docserver_url);
    let secret = quote(jwt_secret);
    let ws = quote(workspace_dir);
    let mut out = String::with_capacity(text.len() + 96);
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        // 只认「键:」开头的行；其余行（注释、insert 头）原样保留
        let replaced: Option<String> = if trimmed.starts_with("docserverUrl:") {
            Some(format!("docserverUrl: '{url}'"))
        } else if trimmed.starts_with("jwtSecret:") {
            Some(format!("jwtSecret: '{secret}'"))
        } else if trimmed.starts_with("docPort:") {
            Some(format!("docPort: {doc_port}"))
        } else if trimmed.starts_with("workspaceDir:") {
            Some(format!("workspaceDir: '{ws}'"))
        } else {
            None
        };
        match replaced {
            Some(line) => {
                out.push_str(indent);
                out.push_str(&line);
                out.push('\n');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// 把四个实际值写进已释放的 cordis.patch.yml（幂等）。
fn write_office_bundle_config(
    bun: &Path,
    docserver_url: &str,
    jwt_secret: &str,
    doc_port: u16,
    workspace_dir: &str,
) -> Result<(), String> {
    let patch = bun.join("cordis.patch.yml");
    let text = std::fs::read_to_string(&patch)
        .map_err(|e| i18n::fmt("office_write_fail", &[&patch.to_string_lossy(), &e.to_string()]))?;
    let out = patch_office_bundle_config(&text, docserver_url, jwt_secret, doc_port, workspace_dir);
    std::fs::write(&patch, out).map_err(|e| {
        i18n::fmt("office_write_fail", &[&patch.to_string_lossy(), &e.to_string()])
    })
}

/// 可由【任意一份】cordis.patch.yml 判断的配置可用性（profile 副本 / 暂存副本通用）。
fn office_config_usable_file(patch: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(patch) else {
        return false;
    };
    let url = crate::py_bridge::read_bundle_scalar(&text, "docserverUrl").unwrap_or_default();
    let secret = crate::py_bridge::read_bundle_scalar(&text, "jwtSecret").unwrap_or_default();
    let port = crate::py_bridge::read_bundle_scalar(&text, "docPort").unwrap_or_default();
    let ws = crate::py_bridge::read_bundle_scalar(&text, "workspaceDir").unwrap_or_default();
    // ⚠ 括号不能省：&& 优先级高于 ||，不写括号会变成
    // `http || (https && secret && port && ws)`——默认 http 配置下后半段全部
    // 被短路掉、恒返回 true，「配置失效自愈」就永远不触发（复查抓出的真 bug）。
    (url.starts_with("http://127.0.0.1:") || url.starts_with("https://127.0.0.1:"))
        && !secret.is_empty()
        && port.parse::<u16>().map(|p| p > 0).unwrap_or(false)
        && !ws.is_empty()
        && Path::new(&ws).is_dir()
}

/// 已释放的 bundle 配置是否可用（照 py_bridge::bridge_config_usable 的口径）。
fn office_config_usable(bun: &Path) -> bool {
    office_config_usable_file(&bun.join("cordis.patch.yml"))
}

/// profile 里**已安装副本**的 cordis.patch.yml —— 那才是 DSH 真正加载的那份。
/// `%DSH_PROFILE_DIR%` 直接给的是 profile 目录本身；退路是 `DSH_HOME\profiles\<name>`。
fn profile_bundle_patch(profile: &str) -> Option<PathBuf> {
    let dir = match std::env::var("DSH_PROFILE_DIR").ok().filter(|s| !s.trim().is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var("DSH_HOME").ok().filter(|s| !s.trim().is_empty()) {
            Some(home) => PathBuf::from(home).join("profiles").join(profile),
            None => return None,
        },
    };
    Some(
        dir.join("node_modules")
            .join("@local")
            .join("dsh-desktop-office-bridge")
            .join("cordis.patch.yml"),
    )
}

/// 文档落盘目录。优先级（**顺序很要紧**）：
///   ① profile 里已安装副本的 workspaceDir —— 那是当前真正在用的值；
///   ② APPDATA 暂存副本的 workspaceDir；
///   ③ 默认 `%USERPROFILE%\Documents\DSH-Office`。
///
/// ⚠ 为什么必须有 ①：1.6.3 首次启动时 activate_pending 会把模板（四键全空）释放到
/// APPDATA 暂存目录，再读暂存副本来定目录 —— 只读 ② 的话永远拿到空值，于是文档
/// 目录被静默改成默认目录（真机事故 2026-11-05：`D:\DSH\Workspace\office-docs`
/// 被换成 `%USERPROFILE%\Documents\DSH-Office`，用户已有的文档"消失"）。
fn resolve_workspace_dir(app: &AppHandle, bun: &Path) -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(profile) = py_bridge_probe_profile(app) {
        if let Some(patch) = profile_bundle_patch(&profile) {
            candidates.push(patch);
        }
    }
    candidates.push(bun.join("cordis.patch.yml"));
    for patch in candidates {
        if let Ok(text) = std::fs::read_to_string(&patch) {
            if let Some(ws) = crate::py_bridge::read_bundle_scalar(&text, "workspaceDir") {
                if !ws.trim().is_empty() {
                    return ws;
                }
            }
        }
    }
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    if home.is_empty() {
        return std::env::temp_dir().join("DSH-Office").to_string_lossy().to_string();
    }
    PathBuf::from(home).join("Documents").join("DSH-Office").to_string_lossy().to_string()
}

// ---------- pending 标记（config 单键写 + 正式字段双保险） ----------

fn set_office_pending(app: &AppHandle, value: &str) -> Result<(), String> {
    config::write_config_key(app, PENDING_KEY, value)
}

fn clear_office_pending(app: &AppHandle) -> Result<(), String> {
    config::write_config_key(app, PENDING_KEY, "")
}

// ---------- 注册 / 摘除（照 py_bridge::register_bundle / remove_bundle） ----------

/// `npm pack` → `dsh plugin --profile <p> add file:<tgz>`。
/// **必须**是 tgz 的 file: 路径 —— add 目录会被 pnpm 装成 link: 软链，
/// DSH 解析不到（py_bridge.rs:867-880 的真机教训）。
fn register_office_bundle(app: &AppHandle, profile: &str, bun: &Path) -> Result<(), String> {
    if !crate::py_bridge::profile_name_usable(profile) {
        return Err(i18n::fmt("office_bad_profile", &[&profile]));
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
        return Err(i18n::fmt("office_no_dsh", &[&hint]));
    };
    let tgz = crate::py_bridge::pack_bundle(bun).map_err(|e| {
        i18n::fmt("office_pack_fail", &[&e])
    })?;
    let file_spec = format!("file:{}", tgz.to_string_lossy().replace('\\', "/"));
    args.extend([
        "plugin".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "add".to_string(),
        file_spec,
    ]);
    process::run_cmd_capture(&program, &args, "", Duration::from_secs(10 * 60)).and_then(|(ok, out)| {
        if ok {
            Ok(())
        } else {
            // 只回原始输出尾部，不在这里套文案（调用方还会包一层；两层都格式化
            // 会在日志里出现「失败：失败：…」套娃，py_bridge 的真机日志原文）
            let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
            Err(tail)
        }
    })
}

/// `dsh plugin --profile <p> remove @local/dsh-desktop-office-bridge`。
/// **必须走 CLI**：bundle 占 package.json dependencies 与 dsh.profile.bundles
/// 两处，手删 node_modules 会留下幽灵清单条目（§6 步骤 1）。
fn remove_office_bundle(app: &AppHandle, profile: &str) -> Result<(), String> {
    if !crate::py_bridge::profile_name_usable(profile) {
        return Err(i18n::fmt("office_bad_profile", &[&profile]));
    }
    let Some((program, mut args)) = crate::py_bridge::dsh_argv(app) else {
        return Err(i18n::fmt("office_no_dsh", &[&"".to_string()]));
    };
    args.extend([
        "plugin".to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "remove".to_string(),
        BUNDLE_PKG_NAME.to_string(),
    ]);
    process::run_cmd_capture(&program, &args, "", Duration::from_secs(10 * 60)).and_then(|(ok, out)| {
        if ok {
            Ok(())
        } else {
            let tail: String = out.lines().rev().take(6).collect::<Vec<_>>().join(" / ");
            Err(tail)
        }
    })
}

// ---------- 任务框架（照 process::spawn_python_task，:4514-4578） ----------

/// 抢 `setup_busy` → 开线程跑任务 → 统一回传 `setup-result` → **无论如何释放 busy**。
/// 与 Python 的安装互斥共用同一把锁：同一时间只允许一个引导级任务（下载 874 MB
/// 的安装器与 pip 全家桶同时跑没有意义，还会把两条进度流搅在一起）。
fn spawn_office_task(
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
        // 前端按钮会**永久**停在「安装中」。GUI 程序没有 stderr，panic 原因必须落盘。
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
                Err(i18n::t("setup_office_panicked").to_string())
            });
        let (ok, msg) = match outcome {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        // 收尾本身也兜一层：记日志/发事件再 panic 时，busy 复位仍然要执行
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(detail) = &panic_detail {
                logger::append_line(
                    &logger::desktop_log_path(&app2),
                    &format!("[setup] Office setup panicked: {detail}"),
                );
            }
            logger::append_line(
                &logger::desktop_log_path(&app2),
                &i18n::fmt(
                    "setup_office_result_line",
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

/// 把一行输出同时送到 desktop.log 与前端 `office-log` 事件。
/// `secret::redact` 是日志双出口的共同前置（token/授权头必须先打码）。
fn office_log_line(app: &AppHandle, line: &str) {
    let shown = crate::secret::redact(line);
    logger::append_line(&logger::desktop_log_path(app), &shown);
    let _ = app.emit("office-log", LogEvent { stream: "office".to_string(), line: shown });
}

// ---------- 安装流水线（§3.3 ①-⑥） ----------

#[tauri::command]
pub async fn setup_install_office(app: AppHandle, dir: Option<String>) -> Result<(), String> {
    // 参数名刻意用单词 `dir`（与 setup_install_node 同一条理由）：多词名会被
    // Tauri 的 camelCase/snake_case 转换绕晕，`Option<String>` 会**静默**变成 None。
    let target = config::validate_office_install_dir(dir.as_deref().unwrap_or(""))?;
    spawn_office_task(app, "office", move |a| office_install_blocking(a, target.as_deref()))
}

fn office_install_blocking(app: &AppHandle, target: Option<&str>) -> Result<String, String> {
    let temp = process::create_private_temp_dir("office")?;
    let result = office_install_inner(app, target, &temp);
    // 874 MB 的安装包与合并产物都不能留在 %TEMP%：成功失败都清
    let _ = std::fs::remove_dir_all(&temp);
    result
}

fn office_install_inner(
    app: &AppHandle,
    target: Option<&str>,
    temp: &Path,
) -> Result<String, String> {
    let cfg = config::load(app);
    let docserver_port = docserver_port_of(&cfg);
    let doc_port = if cfg.office_doc_port > 0 { cfg.office_doc_port } else { DEFAULT_DOC_PORT };

    // ---- ① 前置检查 ----
    let probe = probe_install(app);
    if probe.installed {
        return Err(i18n::fmt(
            "office_already_installed",
            &[&if probe.version.is_empty() { "?".to_string() } else { probe.version.clone() }],
        ));
    }
    // 磁盘余量：874 MB 下载 + 解包后 ~2 GB，3 GB 是硬门槛（不足必然中途失败）
    check_disk_space(target)?;
    // 端口实际试绑（§10.3 坑 1）。3310 只在 DSH 没跑时检查 —— DSH 在跑时它被
    // 我们自己的插件文档服务占着，那是正常状态。
    if !port_bindable(docserver_port) {
        return Err(i18n::fmt("office_port_busy", &[&docserver_port.to_string()]));
    }
    if !process::port_in_use(cfg.port) && !port_bindable(doc_port) {
        return Err(i18n::fmt("office_port_busy", &[&doc_port.to_string()]));
    }
    process::setup_progress(app, "download", &i18n::t("office_step_download"));

    // ---- ② 下载（curl + 断点续传；874 MB，§10.2：中途停摆是常态） ----
    let installer = temp.join("onlyoffice-documentserver.exe");
    download_installer(app, &installer)?;

    // ---- ③ 校验（大小 → SHA-256 → Authenticode，任一不符直接中止） ----
    process::setup_progress(app, "verify", &i18n::t("office_step_verify"));
    let size = std::fs::metadata(&installer).map_err(|e| e.to_string())?.len();
    if size != OFFICE_INSTALLER_BYTES {
        return Err(i18n::fmt(
            "office_verify_size",
            &[&OFFICE_INSTALLER_BYTES.to_string(), &size.to_string()],
        ));
    }
    let hash = sha256_of_large_file(&installer)?;
    if hash != OFFICE_INSTALLER_SHA256 {
        return Err(i18n::fmt(
            "office_verify_hash",
            // 切片结果是不定长的 &str：args 数组的元素要 &&str 才能落到 &dyn Display，
            // 这里干脆转成 String（CI 编译抓出的 E0277）
            &[&OFFICE_INSTALLER_SHA256[..8].to_string(), &hash[..8].to_string()],
        ));
    }
    verify_authenticode(&installer)?;

    // ---- ④ 静默安装（提权一次；/DIR /DS_PORT 均为 P0 实测可用参数） ----
    process::setup_progress(app, "install", &i18n::t("office_step_install"));
    let install_log = temp.join("install.log");
    let body = install_script_body(&installer, target, docserver_port, &install_log);
    let result = elevate_run(&body, Duration::from_secs(45 * 60))?;
    match parse_result_code(&result) {
        Some(0) => {}
        Some(code) => return Err(i18n::fmt("office_install_fail", &[&code.to_string()])),
        // 空结果 = 提权脚本没写回（run_elevated 在 PS 退出时立刻失败，不再干等）
        None if result.trim().is_empty() => {
            return Err(i18n::t("office_elevate_no_result").to_string())
        }
        None => return Err(i18n::fmt("office_install_fail", &[&result.trim()])),
    }
    let probe = probe_install(app);
    if !probe.installed {
        return Err(i18n::t("office_install_verify_fail").to_string());
    }
    let ds_dir = if probe.install_dir.is_empty() {
        target.unwrap_or("").to_string()
    } else {
        probe.install_dir.clone()
    };

    // ---- ⑤ 配置合并（JSON 手术在普通权限侧完成，提权脚本只拷文件 + 重启服务，
    //      §10.2 的教训：提权脚本越简单越不会栽在编码/解析上） ----
    process::setup_progress(app, "config", &i18n::t("office_step_config"));
    let local_json = Path::new(&ds_dir).join("config").join("local.json");
    let existing = std::fs::read_to_string(&local_json).unwrap_or_default();
    let secret = existing_secret_of(&existing).unwrap_or_else(new_office_secret);
    let merged = merge_local_json(&existing, &secret)?;
    let merged_path = temp.join("local.json.merged");
    std::fs::write(&merged_path, merged.as_bytes())
        .map_err(|e| i18n::fmt("office_write_fail", &[&merged_path.to_string_lossy(), &e.to_string()]))?;
    let merge_body = config_script_body(&merged_path, &local_json);
    let result = elevate_run(&merge_body, Duration::from_secs(10 * 60))?;
    if !result.trim().starts_with("OK") {
        return Err(i18n::fmt("office_config_fail", &[&result.trim()]));
    }
    // 健康等待：服务重启到 nginx 真正开始应答有几十秒的窗口
    process::setup_progress(app, "config", &i18n::fmt("office_healthcheck_wait", &[&HEALTH_WAIT_SECS.to_string()]));
    let deadline = Instant::now() + Duration::from_secs(HEALTH_WAIT_SECS);
    let mut healthy = false;
    while Instant::now() < deadline {
        if healthcheck_ok(docserver_port) {
            healthy = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if !healthy {
        return Err(i18n::fmt("office_health_timeout", &[&HEALTH_WAIT_SECS.to_string()]));
    }

    // ---- ⑥ 插件部署（bundle 释放 → 配置 → npm pack → dsh plugin add / pending） ----
    deploy_office_after_install(app, docserver_port, doc_port, &secret)?;
    Ok(i18n::fmt(
        "office_install_done",
        &[&probe.version, &format!("127.0.0.1:{docserver_port}")],
    ))
}

/// 把 bundle 写进磁盘、写好配置，然后视 DSH 状态就地注册或留 pending（§3.3 ⑥）。
/// 安装与「进门自愈」共用这一段（activate_pending 走的是它的后半段）。
fn deploy_office_after_install(
    app: &AppHandle,
    docserver_port: u16,
    doc_port: u16,
    secret: &str,
) -> Result<(), String> {
    let cfg_dir = config::config_dir(app);
    let bun = bundle_dir(&cfg_dir);
    release_bundle_sources(&bun)?;
    process::setup_progress(app, "deploy", &i18n::fmt("office_released", &[&bun.to_string_lossy()]));

    let docserver_url = {
        let cfg = config::load(app);
        if cfg.office_docserver_url.trim().is_empty() {
            format!("http://127.0.0.1:{docserver_port}")
        } else {
            cfg.office_docserver_url.trim().to_string()
        }
    };
    let workspace = resolve_workspace_dir(app, &bun);
    write_office_bundle_config(&bun, &docserver_url, secret, doc_port, &workspace)?;
    process::setup_progress(app, "deploy", &i18n::fmt("office_bundle_configured", &[&docserver_url, &doc_port.to_string()]));

    // 竞态纪律（与 py_bridge::deploy_after_install 同款）：只在 DSH **没在跑**时
    // 就地注册；在跑就留 pending，等进入 DSH 的钩子（set_status → activate_pending）。
    let profile = py_bridge_probe_profile(app).unwrap_or_else(|| {
        logger::append_line(
            &logger::desktop_log_path(app),
            "[office-bridge] probe_profile failed; falling back to profile \"web\"",
        );
        "web".to_string()
    });
    if crate::py_bridge::dsh_port_busy(app) {
        set_office_pending(app, &format!("docserver:{docserver_port}"))?;
        let line = i18n::fmt("office_deferred", &[&profile]);
        process::setup_progress(app, "deploy", &line);
        return Ok(());
    }
    match register_office_bundle(app, &profile, &bun) {
        Ok(()) => {
            clear_office_pending(app)?;
            let line = i18n::fmt("office_registered", &[&profile]);
            process::setup_progress(app, "deploy", &line);
            Ok(())
        }
        Err(e) => {
            // 保留 pending：下次进入 DSH 还会再试一次（activate_pending）
            let _ = set_office_pending(app, &format!("docserver:{docserver_port}"));
            let line = i18n::fmt("office_register_fail", &[&e]);
            process::setup_progress(app, "deploy", &line);
            logger::append_line(
                &logger::desktop_log_path(app),
                &format!("[office-bridge] register_office_bundle failed: {e}"),
            );
            Ok(())
        }
    }
}

// ---------- 下载（curl + 断点续传，§10.2：874 MB 中途停摆是常态） ----------

fn download_installer(app: &AppHandle, dest: &Path) -> Result<(), String> {
    let Some(curl) = crate::detect::where_lookup("curl.exe") else {
        return Err(i18n::t("setup_no_curl").to_string());
    };
    let total = process::probe_content_length(&curl, OFFICE_DOWNLOAD_URL);
    let mut resume_failed = false;
    for attempt in 1u32..=10 {
        // 半截文件已到目标大小：不需要再下（上次刚好在结尾断掉）
        if dest.exists()
            && std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) == OFFICE_INSTALLER_BYTES
        {
            break;
        }
        let mut args: Vec<String> = vec![
            "-fL".into(),
            // 只允许 https，重定向后也只允许（M-3，与 process::try_download_curl 同款）
            "--proto".into(),
            "=https".into(),
            "--proto-redir".into(),
            "=https".into(),
            "--connect-timeout".into(),
            "15".into(),
            // 停摆检测：持续 60 秒速度低于 10 KB/s 就退出重试（Range 续传接着来）
            "--speed-limit".into(),
            "10240".into(),
            "--speed-time".into(),
            "60".into(),
            "-sS".into(),
        ];
        let use_resume = dest.exists() && !resume_failed;
        if use_resume {
            // 断点续传：curl 自己发 Range 请求
            args.push("-C".into());
            args.push("-".into());
        }
        args.push("-o".into());
        args.push(dest.to_string_lossy().to_string());
        args.push(OFFICE_DOWNLOAD_URL.to_string());

        let mut cmd = std::process::Command::new(&curl);
        for a in &args {
            cmd.arg(a);
        }
        process::apply_no_window(&mut cmd);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(e.to_string()),
        };
        let mut last_report = Instant::now();
        let outcome: Result<std::process::ExitStatus, std::io::Error> = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => {
                    if last_report.elapsed() >= Duration::from_millis(800) {
                        last_report = Instant::now();
                        let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                        process::emit_download_progress(app, done, total);
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => break Err(e),
            }
        };
        let mut err_pipe = child.stderr.take();
        let err_text = process::decode_console_output(&process::drain_pipe(&mut err_pipe));
        match outcome {
            Ok(st) if st.success() => {
                let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                process::emit_download_progress(app, done, total);
                return Ok(());
            }
            Ok(_) => {
                if use_resume {
                    // 服务端不支持 Range（416/33）之类：删掉半截文件，整段重来
                    resume_failed = true;
                    let _ = std::fs::remove_file(dest);
                }
                // 落一行停摆/失败原因到进度区，继续下一次尝试
                let tail = err_text.trim().to_string();
                process::setup_progress(
                    app,
                    "download",
                    &i18n::fmt("office_dl_retry", &[&attempt.to_string(), &tail]),
                );
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err(i18n::fmt(
        "office_dl_fail",
        // t() 返回 &'static str：args 元素必须再套一层引用（CI 编译抓出的 E0277）
        &[&i18n::t("office_dl_attempts"), &OFFICE_MANUAL_PAGE],
    ))
}

// ---------- 提权脚本（全部 ASCII 结构 + 路径经单引号转义；BOM 在 run_elevated 里加） ----------

/// 提权执行 + 结果解析的三态归一：None（UAC 被拒）→ 明确错误；
/// 结果文本交给调用方解析。
fn elevate_run(script_body: &str, timeout: Duration) -> Result<String, String> {
    crate::elevate::run_elevated(script_body, timeout)
        .ok_or_else(|| i18n::t("office_elevate_denied").to_string())
}

/// `Start-Process -Wait` 一个 Inno Setup 安装器，退出码写结果文件。
/// /DIR /DS_PORT /SILENT /LOG 都是 P0 实测生效的参数（§10.2）。
fn install_script_body(exe: &Path, dir: Option<&str>, port: u16, log: &Path) -> String {
    let psq = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let mut args: Vec<String> = vec!["/SILENT".to_string()];
    if let Some(d) = dir {
        // 单引号 PS 字面量里的双引号是字面量：Start-Process 拼命令行后
        // Inno 收到 /DIR="C:\my dir"（含空格路径必需）
        args.push(format!("/DIR=\"{}\"", d.replace('\'', "''")));
    }
    args.push(format!("/DS_PORT={port}"));
    args.push(format!("/LOG=\"{}\"", log.to_string_lossy()));
    let arg_list = args.iter().map(|a| psq(a)).collect::<Vec<_>>().join(",");
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         try {{\n\
         \x20 $p = Start-Process -FilePath {exe} -ArgumentList {args} -Wait -PassThru\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value (\"CODE`t\" + $p.ExitCode) -Encoding UTF8\n\
         }} catch {{\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value (\"ERR`t\" + $_.Exception.Message) -Encoding UTF8\n\
         }}\n",
        exe = psq(&exe.to_string_lossy()),
        args = arg_list,
    )
}

/// 备份 local.json → 拷入合并结果 → 重启 ONLYOFFICE 服务（§3.3 ⑤ 的官方命令）。
/// 脚本里没有 JSON 手术 —— 那是 P0 踩过坑的地方（提权脚本必须最小化）。
fn config_script_body(merged: &Path, local_json: &Path) -> String {
    let psq = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = format!("{}.bak-dsh-{}", local_json.to_string_lossy(), stamp);
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         try {{\n\
         \x20 if (Test-Path -LiteralPath {dst}) {{ Copy-Item -LiteralPath {dst} -Destination {bak} -Force }}\n\
         \x20 Copy-Item -LiteralPath {src} -Destination {dst} -Force\n\
         \x20 Get-Service | Where-Object {{ $_.DisplayName -like '*ONLYOFFICE*' }} | Restart-Service -Force\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value 'OK' -Encoding UTF8\n\
         }} catch {{\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value ('ERR|' + $_.Exception.Message) -Encoding UTF8\n\
         }}\n",
        dst = psq(&local_json.to_string_lossy()),
        bak = psq(&backup),
        src = psq(&merged.to_string_lossy()),
    )
}

/// 结果文件的第一行：`CODE\t<退出码>`（OK 之外的一切都当失败）。
fn parse_result_code(text: &str) -> Option<i64> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix("CODE\t")?;
    rest.trim().parse::<i64>().ok()
}

// ---------- 卸载（§6 四步 + §6 修订的两段式清扫） ----------

#[tauri::command]
pub async fn setup_uninstall_office(app: AppHandle) -> Result<(), String> {
    spawn_office_task(app, "office", |a| office_uninstall_blocking(a))
}

fn office_uninstall_blocking(app: &AppHandle) -> Result<String, String> {
    let mut notes: Vec<String> = Vec::new();

    // ---- ① 摘插件（profile 用注册时探测到的那个，不猜） ----
    let cfg_dir = config::config_dir(app);
    let bun = bundle_dir(&cfg_dir);
    match py_bridge_probe_profile(app) {
        Some(profile) => match remove_office_bundle(app, &profile) {
            Ok(()) => notes.push(i18n::fmt("office_removed", &[&profile])),
            Err(e) => notes.push(i18n::fmt("office_remove_fail", &[&profile, &e])),
        },
        None => notes.push(i18n::t("office_no_profile").to_string()),
    }

    // ---- ② 清 pending 标记 + 删释放目录 ----
    clear_office_pending(app)?;
    match remove_bundle_dir(&bun) {
        Ok(()) => notes.push(i18n::fmt("office_dir_removed", &[&bun.to_string_lossy()])),
        Err((e, locked)) => {
            let key = if locked { "office_dir_remove_locked" } else { "office_dir_remove_fail" };
            notes.push(i18n::fmt(key, &[&bun.to_string_lossy(), &e]));
        }
    }

    // ---- ③ 提权跑官方卸载器（/VERYSILENT /NORESTART，P0 实测退出码 0） ----
    let probe = probe_install(app);
    if probe.installed {
        // 把安装目录记进 config：两段式的「清扫残余」（office_cleanup_blocking）
        // 靠它定位残留 —— 手动装的 DS 在 config 里本来没有这个字段，
        // 不在这里补记的话，清扫按钮会"成功"却什么都没删（真机教训 2026-11-06）。
        if !probe.install_dir.is_empty() {
            let _ = config::write_config_key(app, "office_install_dir", &probe.install_dir);
        }
        let uninstaller = if probe.uninstaller.is_empty() {
            if probe.install_dir.is_empty() {
                String::new()
            } else {
                Path::new(&probe.install_dir).join("unins000.exe").to_string_lossy().to_string()
            }
        } else {
            probe.uninstaller.clone()
        };
        if uninstaller.is_empty() || !Path::new(&uninstaller).is_file() {
            notes.push(i18n::t("office_uninstall_missing").to_string());
        } else {
            let body = uninstall_script_body(&uninstaller);
            let result = elevate_run(&body, Duration::from_secs(30 * 60))?;
            match parse_result_code(&result) {
                Some(0) => notes.push(i18n::t("office_uninstalled_ds").to_string()),
                Some(code) => {
                    // 卸载器失败不回滚前两步（已摘干净的部分如实报告，§6）
                    notes.push(i18n::fmt("office_uninstall_fail", &[&code.to_string()]));
                }
                // 空结果 = 提权脚本没写回（run_elevated 在 PS 退出时立刻失败）。
                // 卸载器可能**其实已经跑成**——让用户先核实现场，而不是空口报错。
                None if result.trim().is_empty() => {
                    notes.push(i18n::t("office_elevate_no_result").to_string());
                }
                None => notes.push(i18n::fmt("office_uninstall_fail", &[&result.trim()])),
            }
        }
    } else {
        notes.push(i18n::t("office_uninstall_skip_ds").to_string());
    }

    // ---- ④ 清 config 的 office 字段（load → 归零 → save，类型安全） ----
    // ⚠ `office_install_dir` **刻意不清**：两段式的「清扫残余」还要靠它定位残留，
    //    它会在清扫成功后由 cleanup 自己清掉。其余字段照常归零。
    if let Err(e) = clear_office_config(app) {
        notes.push(i18n::fmt("office_config_reset_fail", &[&e]));
    }
    // 提醒两段式：官方卸载器会留 ~33MB 静态资源 + %ProgramData%\ONLYOFFICE 运行数据（§6）
    notes.push(i18n::t("office_uninstall_leftovers_hint").to_string());
    Ok(notes.join("\n"))
}

fn uninstall_script_body(uninstaller: &str) -> String {
    let psq = |s: &str| format!("'{}'", s.replace('\'', "''"));
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         try {{\n\
         \x20 $p = Start-Process -FilePath {u} -ArgumentList '/VERYSILENT','/NORESTART' -Wait -PassThru\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value (\"CODE`t\" + $p.ExitCode) -Encoding UTF8\n\
         }} catch {{\n\
         \x20 Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Value (\"ERR`t\" + $_.Exception.Message) -Encoding UTF8\n\
         }}\n",
        u = psq(uninstaller),
    )
}

/// 清 config.json 的 office 字段（不碰用户其它设置；类型安全，不走单键写 ——
/// write_config_key 只会写字符串，写进 u16 字段会把整份配置打回默认值）。
fn clear_office_config(app: &AppHandle) -> Result<(), String> {
    let mut c = config::load(app);
    c.office_install_dir = String::new();
    c.office_docserver_url = String::new();
    c.office_bridge_pending = String::new();
    c.office_docserver_port = DEFAULT_DOCSERVER_PORT;
    c.office_doc_port = DEFAULT_DOC_PORT;
    config::save(app, &c)
}

/// 两段式卸载的第二段：清扫官方卸载器留下的残余（§6 修订）。
/// 只接受两个来源的路径：config 里记录过的安装目录、`%ProgramData%\ONLYOFFICE`。
#[tauri::command]
pub async fn setup_office_cleanup(app: AppHandle) -> Result<(), String> {
    spawn_office_task(app, "office", |a| office_cleanup_blocking(a))
}

fn office_cleanup_blocking(app: &AppHandle) -> Result<String, String> {
    let cfg = config::load(app);
    let mut paths: Vec<String> = Vec::new();
    let install_dir = cfg.office_install_dir.trim().to_string();
    if !install_dir.is_empty() {
        paths.push(install_dir);
    }
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    paths.push(PathBuf::from(program_data).join("ONLYOFFICE").to_string_lossy().to_string());
    // 提权前的白名单（照 elevate::is_safe_target 的思路：..、通配符一律拒绝；
    // 空路径拒绝）。这些路径要么是本模块写进 config 的，要么是常量拼出来的，
    // 出现危险形状 = 代码缺陷，宁可不动手。
    for p in &paths {
        if p.is_empty() || p.contains("..") || p.contains('*') || p.contains('?') {
            return Err(i18n::fmt("office_cleanup_bad_path", &[p]));
        }
    }
    let mut body = String::from("$ErrorActionPreference = 'SilentlyContinue'\n");
    body.push_str("$out = New-Object System.Collections.ArrayList\n");
    for p in &paths {
        // 单引号 PS 字面量里的 `'` 写成 `''`（与其它脚本生成函数同一规则）
        let single = p.replace('\'', "''");
        body.push_str(&format!(
            "$p = '{single}'\n\
             if (Test-Path -LiteralPath $p) {{\n\
             \x20 try {{ Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction Stop\n\
             \x20   if (Test-Path -LiteralPath $p) {{ [void]$out.Add(\"ERR`t\" + $p + \"`tstill exists\") }}\n\
             \x20   else {{ [void]$out.Add(\"OK`t\" + $p) }}\n\
             \x20 }} catch {{ [void]$out.Add(\"ERR`t\" + $p + \"`t\" + $_.Exception.Message) }}\n\
             }} else {{ [void]$out.Add(\"OK`t\" + $p) }}\n"
        ));
    }
    body.push_str("$out -join \"`n\" | Set-Content -LiteralPath $env:DSH_ELEVATE_RESULT -Encoding UTF8\n");
    let result = elevate_run(&body, Duration::from_secs(15 * 60))?;
    let failed: Vec<&str> = result.lines().filter(|l| l.starts_with("ERR\t")).collect();
    if failed.is_empty() {
        Ok(i18n::fmt("office_cleanup_done", &[&paths.join(", ")]))
    } else {
        Err(i18n::fmt("office_cleanup_fail", &[&failed.join(" / ")]))
    }
}

// ---------- 进门自愈（set_status 的 status=="running" 钩子；照 py_bridge::activate_pending） ----------

/// 三选一进门（pending / 已释放 / 已注册），三条全不满足零开销返回；
/// **再加一条硬前提**：Document Server 必须在。没有它，注册出来的插件六个工具
/// 全部不可用，还把「卸载后残留标记」变成复活路径 —— 这是 office 与 py 的
/// 本质差别（py 的运行时是 pip 包，独立于安装目录存在）。
pub fn activate_pending(app: &AppHandle) -> Option<String> {
    let cfg_dir = config::config_dir(app);
    let bun = bundle_dir(&cfg_dir);
    let pending = config::read_config_flag(app, PENDING_KEY);
    let released = bundle_released(&bun);
    let profile_hint = py_bridge_probe_profile(app);
    let registered = profile_hint.as_deref().map(py_bridge_bundle_registered).unwrap_or(false);
    if !pending && !released && !registered {
        return None;
    }
    let probe = probe_install(app);
    if !probe.installed {
        if pending {
            logger::append_line(
                &logger::desktop_log_path(app),
                "[office-bridge] pending 标记存在但 OnlyOffice 未安装，跳过激活",
            );
        }
        return None;
    }

    // pending 在但代码被清过（用户动过 %APPDATA%）：重新释放，别让标记永远悬空
    if !released {
        if let Err(e) = release_bundle_sources(&bun) {
            logger::append_line(
                &logger::desktop_log_path(app),
                &format!("[office-bridge] re-release failed: {e}"),
            );
            return None;
        }
    }
    let profile = profile_hint.unwrap_or_else(|| {
        logger::append_line(
            &logger::desktop_log_path(app),
            "[office-bridge] probe_profile failed; falling back to profile \"web\"",
        );
        "web".to_string()
    });

    // 已注册 ≠ 还能用：换端口 / 换安装目录后，profile 里那份 cordis.patch.yml
    // 副本还带着旧值。判据失败就走重写配置 + 重注册（改源目录没用，
    // DSH 加载的是 profile 里那份安装副本 —— py_bridge.rs:630-640 的教训）。
    //
    // ⚠ 判据要读 **profile 副本**，不是 APPDATA 暂存副本：暂存副本是刚由模板
    // （四键全空）释放出来的，拿它判几乎必然"失效"，于是每次启动都无谓重注册、
    // 还把 workspaceDir 重置成默认目录（真机事故 2026-11-05）。
    if registered {
        let installed = profile_bundle_patch(&profile);
        let usable = match &installed {
            Some(p) if p.is_file() => office_config_usable_file(p),
            _ => office_config_usable(&bun),
        };
        if usable {
            if pending {
                let _ = clear_office_pending(app);
            }
            return None;
        }
        let secret = resolve_secret(&bun, &probe.install_dir);
        let docserver_port = docserver_port_of(&config::load(app));
        let doc_port = config::load(app).office_doc_port;
        if let Err(e) = write_office_bundle_config(
            &bun,
            &format!("http://127.0.0.1:{docserver_port}"),
            &secret,
            if doc_port > 0 { doc_port } else { DEFAULT_DOC_PORT },
            &resolve_workspace_dir(app, &bun),
        ) {
            let line = format!("[office-bridge] 重写插件配置失败：{e}");
            logger::append_line(&logger::desktop_log_path(app), &line);
            return Some(line);
        }
        return match register_office_bundle(app, &profile, &bun) {
            Ok(()) => {
                let _ = clear_office_pending(app);
                let line = "[office-bridge] 已注册但配置失效，已重新注册以刷新配置".to_string();
                logger::append_line(&logger::desktop_log_path(app), &line);
                Some(line)
            }
            Err(e) => {
                let line = format!("[office-bridge] 配置失效且重新注册失败：{e}");
                logger::append_line(&logger::desktop_log_path(app), &line);
                Some(line)
            }
        };
    }

    let secret = resolve_secret(&bun, &probe.install_dir);
    let cfg = config::load(app);
    let docserver_port = docserver_port_of(&cfg);
    let doc_port = if cfg.office_doc_port > 0 { cfg.office_doc_port } else { DEFAULT_DOC_PORT };
    if let Err(e) = write_office_bundle_config(
        &bun,
        &format!("http://127.0.0.1:{docserver_port}"),
        &secret,
        doc_port,
        &resolve_workspace_dir(app, &bun),
    ) {
        logger::append_line(
            &logger::desktop_log_path(app),
            &format!("[office-bridge] write_office_bundle_config failed: {e}"),
        );
        return None;
    }
    match register_office_bundle(app, &profile, &bun) {
        Ok(()) => {
            let _ = clear_office_pending(app);
            let line = i18n::fmt("office_registered", &[&profile]);
            logger::append_line(&logger::desktop_log_path(app), &format!("[office-bridge] {line}"));
            Some(line)
        }
        Err(e) => {
            // 不清标记：下次进入 DSH 还会再试
            let line = i18n::fmt("office_register_fail", &[&e]);
            logger::append_line(&logger::desktop_log_path(app), &format!("[office-bridge] {line}"));
            Some(line)
        }
    }
}

/// JWT 密钥的取用顺序：已释放配置里的（保持插件配置稳定）→
/// DS local.json 里的（重装不废配置）→ 现生成。
fn resolve_secret(bun: &Path, install_dir: &str) -> String {
    if let Ok(text) = std::fs::read_to_string(bun.join("cordis.patch.yml")) {
        if let Some(s) = crate::py_bridge::read_bundle_scalar(&text, "jwtSecret") {
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    if !install_dir.is_empty() {
        let local = Path::new(install_dir).join("config").join("local.json");
        if let Ok(text) = std::fs::read_to_string(&local) {
            if let Some(s) = existing_secret_of(&text) {
                return s;
            }
        }
    }
    new_office_secret()
}

// ---------- 磁盘空间（① 的硬门槛） ----------

fn check_disk_space(target: Option<&str>) -> Result<(), String> {
    // 目标盘符：用户所选目录的盘符，未填则默认安装落 C 盘
    let letter = match target {
        Some(d) if d.len() >= 2 && d.as_bytes()[1] == b':' => d.as_bytes()[0],
        _ => b'C',
    };
    if !letter.is_ascii_alphabetic() {
        return Ok(()); // 形状不对就不猜（校验器在命令入口已经挡过一轮）
    }
    let script = format!(
        "[System.IO.DriveInfo]::new('{}').AvailableFreeSpace.ToString([System.Globalization.CultureInfo]::InvariantCulture)",
        letter as char
    );
    let out = match run_powershell(&script, Duration::from_secs(20)) {
        Ok(o) => o.trim().to_string(),
        Err(e) => return Err(i18n::fmt("office_disk_probe_fail", &[&e])),
    };
    match out.parse::<u64>() {
        Ok(free) if free >= 3 * 1024 * 1024 * 1024 => Ok(()),
        Ok(free) => Err(i18n::fmt("office_disk_low", &[&format_mb(free)])),
        Err(_) => Err(i18n::fmt("office_disk_probe_fail", &[&out])),
    }
}

fn format_mb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1073741824.0)
}

// ---------- 纯函数单测（CI 的 rust-test 通道跑这些；本机没有 Rust 工具链） ----------

#[cfg(test)]
mod tests {
    use super::*;

    const REG_SAMPLE: &str = "\
HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ONLYOFFICE DocumentServer_is1\r\n\
    DisplayName    REG_SZ    ONLYOFFICE Document Server 10.0.0\r\n\
    DisplayVersion    REG_SZ    10.0.0.169\r\n\
    InstallLocation    REG_SZ    D:\\Programs\\ONLYOFFICE\\DocumentServer\\\r\n\
    UninstallString    REG_SZ    \"D:\\Programs\\ONLYOFFICE\\DocumentServer\\unins001.exe\"\r\n";

    #[test]
    fn parse_uninstall_reg_extracts_three_values() {
        let (version, location, uninstaller) = parse_uninstall_reg(REG_SAMPLE);
        assert_eq!(version, "10.0.0.169");
        assert_eq!(location, "D:\\Programs\\ONLYOFFICE\\DocumentServer");
        // UninstallString 的引号在解析层剥掉：调用方要的是能直接 is_file 的裸路径
        assert_eq!(uninstaller, "D:\\Programs\\ONLYOFFICE\\DocumentServer\\unins001.exe");
    }

    #[test]
    fn parse_uninstall_reg_tolerates_garbage() {
        let (v, l, u) = parse_uninstall_reg("ERROR: The system was unable to find");
        assert!(v.is_empty() && l.is_empty() && u.is_empty());
    }

    #[test]
    fn merge_local_json_on_empty_keeps_required_keys() {
        let out = merge_local_json("", "abc123").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["request-filtering-agent"]["allowPrivateIPAddress"], true);
        assert_eq!(v["services"]["CoAuthoring"]["token"]["enable"]["browser"], true);
        assert_eq!(
            v["services"]["CoAuthoring"]["secret"]["inbox"]["string"],
            "abc123"
        );
        assert_eq!(
            v["services"]["CoAuthoring"]["secret"]["session"]["string"],
            "abc123"
        );
    }

    #[test]
    fn merge_local_json_preserves_installer_keys() {
        let existing = r#"{
  "services": { "CoAuthoring": { "server": { "port": "8000" } } },
  "license": { "license_file": "C:/ProgramData/ONLYOFFICE/Data/license.lic" },
  "wopi": { "enable": false }
}"#;
        let out = merge_local_json(existing, "s3cret").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // 安装器写进去的键全部保留
        assert_eq!(v["services"]["CoAuthoring"]["server"]["port"], "8000");
        assert_eq!(v["license"]["license_file"], "C:/ProgramData/ONLYOFFICE/Data/license.lic");
        assert_eq!(v["wopi"]["enable"], false);
        // 需要的键全部就位
        assert_eq!(v["services"]["CoAuthoring"]["secret"]["outbox"]["string"], "s3cret");
        assert_eq!(v["request-filtering-agent"]["allowMetaIPAddress"], true);
    }

    #[test]
    fn merge_local_json_tolerates_non_object_nodes() {
        // 安装器某天把 services 写成字符串也不该 panic
        let out = merge_local_json(r#"{"services": "oops"}"#, "k").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["services"]["CoAuthoring"]["secret"]["inbox"]["string"], "k");
    }

    #[test]
    fn merge_local_json_rejects_garbage() {
        assert!(merge_local_json("{not json", "k").is_err());
    }

    #[test]
    fn existing_secret_finds_inbox_string() {
        let text = r#"{"services":{"CoAuthoring":{"secret":{"inbox":{"string":"old-key"}}}}}"#;
        assert_eq!(existing_secret_of(text).as_deref(), Some("old-key"));
        assert_eq!(existing_secret_of("{\"services\":{}}"), None);
        assert_eq!(existing_secret_of("garbage"), None);
    }

    const PATCH_SAMPLE: &str = "\
# comment kept\n\
- id: dsh-desktop-office-bridge\n\
  config:\n\
    docserverUrl: ''\n\
    jwtSecret: ''\n\
    docPort: 0\n\
    workspaceDir: ''\n";

    #[test]
    fn patch_office_bundle_config_fills_all_four_keys() {
        let out = patch_office_bundle_config(PATCH_SAMPLE, "http://127.0.0.1:8080", "sec", 3310, "D:\\docs");
        assert!(out.contains("docserverUrl: 'http://127.0.0.1:8080'"));
        assert!(out.contains("jwtSecret: 'sec'"));
        assert!(out.contains("docPort: 3310"));
        assert!(out.contains("workspaceDir: 'D:\\docs'"));
        assert!(out.contains("# comment kept"));
        assert!(out.contains("- id: dsh-desktop-office-bridge"));
    }

    #[test]
    fn patch_office_bundle_config_is_idempotent() {
        let once = patch_office_bundle_config(PATCH_SAMPLE, "http://127.0.0.1:8080", "sec", 3310, "D:\\docs");
        let twice = patch_office_bundle_config(&once, "http://127.0.0.1:8080", "sec", 3310, "D:\\docs");
        assert_eq!(once, twice);
    }

    #[test]
    fn patch_office_bundle_config_escapes_single_quotes() {
        let out = patch_office_bundle_config(PATCH_SAMPLE, "http://127.0.0.1:8080", "it's", 3310, "D:\\o'brien");
        assert!(out.contains("jwtSecret: 'it''s'"));
        assert!(out.contains("workspaceDir: 'D:\\o''brien'"));
        // 读回能还原
        assert_eq!(
            crate::py_bridge::read_bundle_scalar(&out, "jwtSecret").as_deref(),
            Some("it's")
        );
    }

    #[test]
    fn office_phase_truth_table() {
        use OfficeBridgePhase::*;
        assert_eq!(office_bridge_phase(false, false), NotInstalled);
        assert_eq!(office_bridge_phase(false, true), NeedsRelease);
        assert_eq!(office_bridge_phase(true, false), PendingActivate);
        assert_eq!(office_bridge_phase(true, true), Active);
    }

    #[test]
    fn parse_result_code_reads_exit_code_line() {
        assert_eq!(parse_result_code("CODE\t0\n"), Some(0));
        assert_eq!(parse_result_code("CODE\t1622\n"), Some(1622));
        assert_eq!(parse_result_code("ERR\tboom\n"), None);
        assert_eq!(parse_result_code(""), None);
    }

    #[test]
    fn install_script_embeds_dir_and_port() {
        let body = install_script_body(
            Path::new("D:\\temp\\onlyoffice.exe"),
            Some("D:\\Programs\\ONLYOFFICE\\DocumentServer"),
            8080,
            Path::new("D:\\temp\\install.log"),
        );
        assert!(body.contains("Start-Process -FilePath 'D:\\temp\\onlyoffice.exe'"));
        assert!(body.contains("/DIR=\"D:\\Programs\\ONLYOFFICE\\DocumentServer\""));
        assert!(body.contains("/DS_PORT=8080"));
        assert!(body.contains("/SILENT"));
        assert!(body.contains("/LOG="));
        // 结构是 ASCII + 单引号转义，绝不能出现会破坏 PS 解析的裸引号
        assert!(body.starts_with("$ErrorActionPreference"));
    }

    #[test]
    fn config_script_copies_and_restarts() {
        let body = config_script_body(
            Path::new("C:\\temp\\local.json.merged"),
            Path::new("D:\\DS\\config\\local.json"),
        );
        assert!(body.contains("Copy-Item -LiteralPath 'D:\\DS\\config\\local.json' -Destination 'D:\\DS\\config\\local.json.bak-dsh-"));
        assert!(body.contains("Restart-Service -Force"));
        assert!(body.contains("*ONLYOFFICE*"));
    }

    #[test]
    fn new_office_secret_is_32_hex() {
        let s = new_office_secret();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 回归测试（复查抓出的优先级 bug）：http 配置下 secret/port/workspace 也必须
    /// 参与判定 —— 缺任何一个都算「配置失效」，activate_pending 才会去自愈。
    #[test]
    fn office_config_usable_requires_every_key() {
        let base = std::env::temp_dir().join(format!("dsh-office-usable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let ws_dir = base.join("docs");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let write = |url: &str, secret: &str, port: &str, ws: &str| {
            std::fs::write(
                base.join("cordis.patch.yml"),
                format!("docserverUrl: '{url}'\njwtSecret: '{secret}'\ndocPort: {port}\nworkspaceDir: '{ws}'\n"),
            )
            .unwrap();
        };
        // 四项齐全 → 可用
        write("http://127.0.0.1:8080", "s3cret", "3310", ws_dir.to_string_lossy().as_ref());
        assert!(office_config_usable(&base));
        // http + secret 为空 → 不可用（正是旧括号写法会漏判的形状）
        write("http://127.0.0.1:8080", "", "3310", ws_dir.to_string_lossy().as_ref());
        assert!(!office_config_usable(&base));
        // http + 端口缺失 → 不可用
        write("http://127.0.0.1:8080", "s3cret", "", ws_dir.to_string_lossy().as_ref());
        assert!(!office_config_usable(&base));
        // http + workspace 不是真实目录 → 不可用
        write("http://127.0.0.1:8080", "s3cret", "3310", base.join("nope").to_string_lossy().as_ref());
        assert!(!office_config_usable(&base));
        // 非 127.0.0.1 的 URL → 不可用
        write("http://10.0.0.1:8080", "s3cret", "3310", ws_dir.to_string_lossy().as_ref());
        assert!(!office_config_usable(&base));
        // yml 缺失 → 不可用
        std::fs::remove_file(base.join("cordis.patch.yml")).unwrap();
        assert!(!office_config_usable(&base));
        let _ = std::fs::remove_dir_all(&base);
    }
}
