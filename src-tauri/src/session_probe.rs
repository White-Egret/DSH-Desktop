//! S2′ 会话探针：从 DSH 自己的会话缓存里读出「有没有正在跑的 Agent 会话」。
//!
//! 为什么不是等 S2 的 HTTP 端点：`/api/desktop/tasks` 上游还没有，而 DSH 会把每个会话的
//! 状态**明文**写在 `<DSH_HOME>\storages\session_projcache\sessions\<id>.json` 里。
//! 判据（`openTurnStartSeq` / `openStep` / `pendingCalls` / `inbox`）与上游官方壳的
//! `hasDesktopActiveTasks` 是同一套 —— 也就是说不用改 DSH、不用解压 `.jsonl.zstd`、
//! 不用走 `/api` 的 typert 协议，读一个 JSON 就能拿到「有数」的答案。
//!
//! **这是内部缓存格式，不是公开契约**。DSH 改结构 → 探针失效 → 退回今天的行为
//! （不拦、能退），不会把应用卡死；日志里留有原因可排障。安全类功能与普通功能相反：
//! 宁可多问一句，也不悄悄丢掉用户半小时的活。

use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// 「最近写过」的窗口。正常 turn 里工具调用与步骤边界都会写盘，30 分钟无写入基本可以
/// 认为已无实质推进。
///
/// 这是全文唯一「拍了个数」的地方：调大 = 崩溃残留多骗一会儿，调小 = 超长纯生成可能漏判。
/// 取 30 分钟是偏保守 —— 宁可多问一句，也不悄悄丢掉用户半小时的活。
pub(crate) const SESSION_ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// 命中活跃的具体依据。记进日志与快照，让「为什么被拦下」有据可查。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Signal {
    OpenTurn,
    OpenStep,
    PendingCalls,
    Inbox,
}

impl Signal {
    /// 日志用的稳定标识（英文短名，与 `EndpointError` 的 Display 同一考虑：
    /// 原因文本只写一份，中英模板复用它）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OpenTurn => "open-turn",
            Self::OpenStep => "open-step",
            Self::PendingCalls => "pending-calls",
            Self::Inbox => "inbox",
        }
    }
}

/// 一个判为活跃的会话。
pub(crate) struct SessionActivity {
    /// 文件名去掉 `.json`（`session-<uuid>`）
    pub session_id: String,
    /// `record.identity.cwd`，弹窗标题用它的末段
    pub cwd: String,
    pub signals: Vec<Signal>,
}

/// 一次探测的结果。三种语义必须分开：`Idle` 是「权威地没有」，`Unavailable` 是「问不到」——
/// 后者绝不能当成前者，否则探针一坏就静默失去保护。
pub(crate) enum ProbeOutcome {
    Active(Vec<SessionActivity>),
    Idle,
    /// 诊断短句，交给 i18n 模板
    Unavailable(&'static str),
}

/// 扫一遍会话缓存。
///
/// `home` 是 **`config::validate_home_dir` 解析后的路径**（家目录可配置，不能自己拼
/// `%USERPROFILE%\.dsh`）。`spawned_at` 是本次 DSH 的启动时刻，用来排除上一代崩溃留下的
/// 未关闭 turn；`window` 是「最近写过」的时限。两者缺一不可，否则会有幽灵任务。
///
/// 性能：**先按 mtime 过滤再 parse**。真机上有 96 份会话，全解析会把 1 Hz 的
/// `get_active_tasks` 拖慢；按 mtime 过滤之后通常只有 1~2 份需要读，其余一个字节都不读。
pub(crate) fn probe(
    home: &Path,
    spawned_at: SystemTime,
    now: SystemTime,
    window: Duration,
) -> ProbeOutcome {
    let dir = home.join("storages").join("session_projcache").join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ProbeOutcome::Unavailable("session cache directory does not exist")
        }
        Err(_) => return ProbeOutcome::Unavailable("session cache directory is unreadable"),
    };

    let mut active: Vec<SessionActivity> = Vec::new();
    let mut bad_json = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // ---- 第一层：mtime 过滤（不读文件内容）----
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if !fresh_enough(mtime, spawned_at, now, window) {
            continue;
        }
        // ---- 第二层：解析（只读新鲜文件）----
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            bad_json += 1;
            continue;
        };
        let Some((signals, cwd)) = judge(&json) else {
            continue;
        };
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        active.push(SessionActivity { session_id, cwd, signals });
    }

    // 「读到的文件全坏」才报不可用：坏一两个属于正常（正在写的文件可能被读到半截），
    // 跳过它们继续处理其它才是对的。
    if active.is_empty() && bad_json >= 3 {
        return ProbeOutcome::Unavailable("all fresh session files failed to parse");
    }
    if active.is_empty() {
        ProbeOutcome::Idle
    } else {
        // 按 session_id 排序：BTreeMap 的快照本身有序，但输入顺序不定会让「首次观察」
        // 的 started_at 分配随 read_dir 的返回顺序漂移，列表看起来在跳。
        active.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        ProbeOutcome::Active(active)
    }
}

/// 两层新鲜度：**晚于本次 DSH 启动** 且 **在窗口内**。
///
/// 两个比较都用 `duration_since` 并把 `Err` 当作「不满足」：系统时钟回拨时
/// `mtime` 可能「晚于」`now`，`duration_since` 会返回 `Err` —— 那种情况按不新鲜处理，
/// 既不会 panic，也不会把时钟回拨误判成「刚刚写过」。
fn fresh_enough(mtime: SystemTime, spawned_at: SystemTime, now: SystemTime, window: Duration) -> bool {
    // 晚于本次 DSH 启动：挡住上一代崩溃留下的 openTurn（文件还在，但早于新实例）
    let after_spawn = mtime
        .duration_since(spawned_at)
        .map(|d| !d.is_zero())
        .unwrap_or(false);
    if !after_spawn {
        return false;
    }
    // 在窗口内：用 `>` 而不是 `>=`，mtime 恰好落在窗口边缘时按「不在窗口内」处理，
    // 避免同一份文件在两轮探测之间来回横跳。
    now.duration_since(mtime)
        .map(|age| age < window)
        .unwrap_or(false)
}

/// 判据：任一命中即算活跃。返回命中的信号与 `identity.cwd`。
///
/// 纯函数，只吃一个 `Value`，便于直接单测。**绝不 `unwrap`**：缓存格式会变，
/// 缺键 / 类型不符一律落进「未命中」，绝不 panic —— 探针 panic 会把整个退出流程带走。
///
/// 入参是**整个文件的顶层**（`{version, record: {identity, rows}}`），
/// 不是 `record` 本身：真机核实过这一层包装（2026-10-31，96 份会话缓存一致）。
pub(crate) fn judge(record: &Value) -> Option<(Vec<Signal>, String)> {
    let record = record.get("record")?;
    let rows = record.get("rows")?;
    let mut signals = Vec::new();

    // turnBoundary.val.openTurnStartSeq != null —— 实测是整数 seq（如 2244）
    if is_present_seq(rows, &["turnBoundary", "val", "openTurnStartSeq"]) {
        signals.push(Signal::OpenTurn);
    }
    // sessionStats.val.openStep != null —— 实测是对象（{turn, step, startTime, …}）
    if is_present_object(rows, &["sessionStats", "val", "openStep"]) {
        signals.push(Signal::OpenStep);
    }
    // sessionStats.val.pendingCalls 非空（对象里至少一个键）
    if let Some(calls) = rows
        .get("sessionStats")
        .and_then(|v| v.get("val"))
        .and_then(|v| v.get("pendingCalls"))
        .and_then(Value::as_object)
    {
        if !calls.is_empty() {
            signals.push(Signal::PendingCalls);
        }
    }
    // inbox.val["next-turn"] 或 ["next-step"] 非空
    if let Some(inbox) = rows
        .get("inbox")
        .and_then(|v| v.get("val"))
        .and_then(Value::as_object)
    {
        let queued = ["next-turn", "next-step"].iter().any(|k| {
            inbox
                .get(*k)
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        });
        if queued {
            signals.push(Signal::Inbox);
        }
    }

    if signals.is_empty() {
        return None;
    }
    let cwd = record
        .get("identity")
        .and_then(|v| v.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((signals, cwd))
}

/// `rows` 里按路径取一个「seq 型」字段并判存在。**只认整数**：实测值恒为整数
/// （如 `2244`），`null` / 字符串 / 布尔 / 数组一律不算。
///
/// 为什么按类型而不是只判 `!is_null`：`false` / `[]` / `{}` 也都不是 `null`，
/// 但它们显然不是「有一个打开的 turn」。只判非 null 会让一个格式变异的文件
/// 把整个会话误判成活跃 —— 那种误判的代价是每次退出都弹窗。
fn is_present_seq(rows: &Value, path: &[&str]) -> bool {
    matches!(dig(rows, path), Some(Value::Number(n)) if n.is_i64() || n.is_u64())
}

/// 同上，但要求是**非空对象**（`openStep` 实测形如 `{turn, step, startTime, …}`）。
fn is_present_object(rows: &Value, path: &[&str]) -> bool {
    matches!(dig(rows, path), Some(Value::Object(m)) if !m.is_empty())
}

/// 按路径下钻取值；路径上任何一环缺失都返回 `None`。
fn dig<'a>(rows: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = rows;
    for key in path {
        cur = cur.get(*key)?;
    }
    Some(cur)
}

/// 把 `cwd` 压成弹窗标题用的末段（`D:\DSH\Workspace` → `Workspace`）。
///
/// 空 cwd 返回空串：调用方据此回落到「不带路径」的标题，而不是显示一个空的冒号。
pub(crate) fn cwd_tail(cwd: &str) -> String {
    cwd.rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ---------- judge：表驱动 ----------

    fn rec_with(rows: Value) -> Value {
        json!({
            "version": 7,
            "record": {
                "identity": { "cwd": "D:\\DSH\\Workspace" },
                "rows": rows,
            }
        })
    }

    #[test]
    fn judge_hits_each_signal() {
        let cases: Vec<(&str, Value, Signal)> = vec![
            (
                "openTurn",
                json!({ "turnBoundary": { "val": { "openTurnStartSeq": 2244 } } }),
                Signal::OpenTurn,
            ),
            (
                "openStep",
                json!({ "sessionStats": { "val": { "openStep": { "turn": 13 } } } }),
                Signal::OpenStep,
            ),
            (
                "pendingCalls",
                json!({ "sessionStats": { "val": { "pendingCalls": { "call_00_x": 1793273594522u64 } } } }),
                Signal::PendingCalls,
            ),
            (
                "next-turn",
                json!({ "inbox": { "val": { "next-turn": [{ "seq": 1 }] } } }),
                Signal::Inbox,
            ),
            (
                "next-step",
                json!({ "inbox": { "val": { "next-step": [{ "seq": 2 }] } } }),
                Signal::Inbox,
            ),
        ];
        for (name, rows, want) in cases {
            let (signals, cwd) = judge(&rec_with(rows)).unwrap_or_else(|| panic!("应命中：{name}"));
            assert!(signals.contains(&want), "{name} 应命中 {want:?}，实际 {signals:?}");
            assert_eq!(cwd, "D:\\DSH\\Workspace");
        }
    }

    #[test]
    fn judge_misses_when_all_clear() {
        // 真机「已结束」的会话长这样：openTurnStartSeq 为 null、openStep 为 null、
        // pendingCalls 空对象、inbox 两个空数组
        let rows = json!({
            "turnBoundary": { "val": { "openTurnStartSeq": null } },
            "sessionStats": { "val": { "openStep": null, "pendingCalls": {} } },
            "inbox": { "val": { "next-turn": [], "next-step": [] } },
        });
        assert!(judge(&rec_with(rows)).is_none());
    }

    #[test]
    fn judge_tolerates_missing_rows() {
        // rows 整体缺失 / 类型不对 / 不是对象：一律 None，绝不 panic
        for payload in [json!({}), json!({ "record": {} }), json!({ "record": { "rows": 42 } })] {
            assert!(judge(&payload).is_none(), "不应 panic 也不应命中：{payload}");
        }
    }

    #[test]
    fn judge_tolerates_wrong_types() {
        // 字段存在但类型变了（DSH 改结构）：按未命中处理，不猜
        let rows = json!({
            "turnBoundary": { "val": { "openTurnStartSeq": "2244" } },
            "sessionStats": { "val": { "pendingCalls": [1, 2, 3], "openStep": false } },
            "inbox": { "val": { "next-turn": "oops" } },
        });
        assert!(judge(&rec_with(rows)).is_none());
    }

    #[test]
    fn cwd_tail_takes_last_segment() {
        assert_eq!(cwd_tail("D:\\DSH\\Workspace"), "Workspace");
        assert_eq!(cwd_tail("/home/me/proj"), "proj");
        assert_eq!(cwd_tail("D:\\DSH\\Workspace\\"), "Workspace");
        assert_eq!(cwd_tail(""), "");
    }

    // ---------- probe：临时目录 ----------

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 造一个 `home/storages/session_projcache/sessions` 目录，返回 (home, sessions_dir)
    fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "dsh-probe-test-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let sessions = home.join("storages").join("session_projcache").join("sessions");
        fs::create_dir_all(&sessions).expect("造临时目录");
        (home, sessions)
    }

    fn write_session(dir: &Path, name: &str, active: bool) {
        let rows = if active {
            json!({
                "turnBoundary": { "val": { "openTurnStartSeq": 2244 } },
                "sessionStats": { "val": { "openStep": null, "pendingCalls": {} } },
                "inbox": { "val": { "next-turn": [], "next-step": [] } },
            })
        } else {
            json!({
                "turnBoundary": { "val": { "openTurnStartSeq": null } },
                "sessionStats": { "val": { "openStep": null, "pendingCalls": {} } },
                "inbox": { "val": { "next-turn": [], "next-step": [] } },
            })
        };
        let doc = json!({
            "version": 7,
            "record": { "identity": { "cwd": "D:\\DSH\\Workspace" }, "rows": rows },
        });
        fs::write(dir.join(name), serde_json::to_string(&doc).unwrap()).expect("写会话文件");
    }

    /// 把文件的 mtime 设成 `age` 之前（用来造「过旧」的样本）
    fn age_file(path: &Path, age: Duration) {
        let target = SystemTime::now()
            .checked_sub(age)
            .expect("时间可回退");
        let f = fs::OpenOptions::new().write(true).open(path).expect("打开");
        f.set_modified(target).expect("改 mtime");
    }

    #[test]
    fn probe_active_when_fresh_and_open() {
        let (home, dir) = scratch("active");
        write_session(&dir, "session-a.json", true);
        let now = SystemTime::now();
        let spawned = now - Duration::from_secs(60);
        match probe(&home, spawned, now, SESSION_ACTIVE_WINDOW) {
            ProbeOutcome::Active(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].session_id, "session-a");
                assert_eq!(v[0].cwd, "D:\\DSH\\Workspace");
                assert!(v[0].signals.contains(&Signal::OpenTurn));
            }
            _ => panic!("应判为活跃"),
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_idle_when_fresh_but_closed() {
        let (home, dir) = scratch("closed");
        write_session(&dir, "session-b.json", false);
        let now = SystemTime::now();
        let spawned = now - Duration::from_secs(60);
        assert!(matches!(
            probe(&home, spawned, now, SESSION_ACTIVE_WINDOW),
            ProbeOutcome::Idle
        ));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_idle_when_open_but_stale() {
        // 崩溃残留：openTurn 还开着，但 40 分钟没写过
        let (home, dir) = scratch("stale");
        write_session(&dir, "session-c.json", true);
        age_file(&dir.join("session-c.json"), Duration::from_secs(40 * 60));
        let now = SystemTime::now();
        let spawned = now - Duration::from_secs(3600);
        assert!(
            matches!(probe(&home, spawned, now, SESSION_ACTIVE_WINDOW), ProbeOutcome::Idle),
            "过旧的会话不该被判为活跃"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_idle_when_mtime_before_spawn() {
        // 上一代崩溃留下的 openTurn：文件很新，但早于本次 DSH 启动
        let (home, dir) = scratch("before-spawn");
        write_session(&dir, "session-d.json", true);
        let now = SystemTime::now();
        let spawned = now + Duration::from_secs(60); // 本次实例「刚启动」
        assert!(
            matches!(probe(&home, spawned, now, SESSION_ACTIVE_WINDOW), ProbeOutcome::Idle),
            "早于 spawn 的会话不该被判为活跃"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_unavailable_when_dir_missing() {
        let home = std::env::temp_dir().join("dsh-probe-test-missing-dir");
        let _ = fs::remove_dir_all(&home);
        let now = SystemTime::now();
        assert!(matches!(
            probe(&home, now, now, SESSION_ACTIVE_WINDOW),
            ProbeOutcome::Unavailable(_)
        ));
    }

    #[test]
    fn probe_skips_bad_json_and_still_finds_active() {
        let (home, dir) = scratch("mixed");
        write_session(&dir, "session-good.json", true);
        fs::write(dir.join("session-bad.json"), b"{ this is not json").expect("写坏文件");
        let now = SystemTime::now();
        let spawned = now - Duration::from_secs(60);
        match probe(&home, spawned, now, SESSION_ACTIVE_WINDOW) {
            ProbeOutcome::Active(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].session_id, "session-good");
            }
            _ => panic!("坏 JSON 应被跳过，好文件仍应命中"),
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn probe_window_boundary_is_exclusive() {
        // mtime 恰好等于 now - window 时按「不在窗口内」（`>` 而非 `>=`）
        let now = SystemTime::now();
        let spawned = now - Duration::from_secs(3600);
        let edge = now - SESSION_ACTIVE_WINDOW;
        assert!(
            !fresh_enough(edge, spawned, now, SESSION_ACTIVE_WINDOW),
            "窗口边缘应排除"
        );
        // 差 1 秒仍在窗口内
        let inside = now - (SESSION_ACTIVE_WINDOW - Duration::from_secs(1));
        assert!(fresh_enough(inside, spawned, now, SESSION_ACTIVE_WINDOW));
    }

    #[test]
    fn fresh_enough_handles_clock_skew() {
        // 时钟回拨：mtime 看起来比 now 还晚 —— duration_since 返回 Err，按「不新鲜」处理
        let now = SystemTime::now();
        let future = now + Duration::from_secs(600);
        let spawned = now - Duration::from_secs(60);
        assert!(!fresh_enough(future, spawned, now, SESSION_ACTIVE_WINDOW));
    }
}
