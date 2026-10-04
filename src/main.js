// DSH Desktop 前端（通过 window.__TAURI__.core.invoke 与 Rust 后端通信）
// 布局：43.2px 工具栏（按钮居左 / 状态一行居右）；工具栏下方在「等待/错误」提示
// 与内嵌 DSH 页面（native webview，盖在本页面之上）之间切换。
// 多语言：静态文案走 data-i18n（见 index.html + i18n.js），动态文案走 I18N.t(key, ...)；
// 界面语言由配置 config.language 决定，保存设置后即时切换；Rust 端日志同样本地化。
const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;
const I18N = window.I18N;
const t = (...args) => I18N.t(...args);

const $ = (id) => document.getElementById(id);

let config = null;        // ConfigReport（含 exists 标志 + 展平的 config 字段 + first_run）
let status = 'idle';      // idle|starting|running|running-external|stopping|error|port-busy|updating
let updating = false;
let checkingVersion = false;
let lastVersionCheckAt = 0;      // 上次版本检查完成时刻（自动检查冷却 60 秒，避免重复请求 npm）
let launchedByAutostart = false; // 本次进程是否由「开机自启」触发（决定静默+延迟策略）
let statusMessage = null;        // 最近一次状态事件携带的附加消息（Rust 端已本地化）
let lastErrorText = '';          // 最近一次错误文本（「复制错误信息」使用）
// 最近一次环境检测结果（detect_environment 的返回）：向导、首选项的 Node 版本行共用。
// 与 wiz.detection 的区别：这个在向导关闭后仍保留，首选项里打开设置时不需要重新检测。
let lastEnvDetection = null;
// ---------- 安全模式状态（编排在 Rust 侧 safe.rs，前端只消费事件） ----------
let safeMode = false;       // 安全实例是否激活（safe-mode-change 事件 / get_safe_status 驱动）
let safeReport = null;      // 最近一次进入的 SafeReport（路径与凭据借用状态；绝不含凭据内容）
let safeBusy = false;       // 进入/退出流程进行中（点击按钮到收到 safe-mode-change 为止）

// 状态键 → 词典 key / 圆点颜色（文案经 t() 取，随语言切换）
const STATUS_META = {
  'idle':             { key: 'st_idle',        dot: 'gray' },
  'starting':         { key: 'st_starting',    dot: 'yellow' },
  'running':          { key: 'st_running',     dot: 'green' },
  'running-external': { key: 'st_running_ext', dot: 'blue' },
  'stopping':         { key: 'st_stopping',    dot: 'yellow' },
  'error':            { key: 'st_error',       dot: 'red' },
  'port-busy':        { key: 'st_port_busy',   dot: 'orange' },
  'updating':         { key: 'st_updating',    dot: 'purple' },
};

// ---------- 日志 ----------

const logBody = $('log');
function appendLog(stream, line) {
  const el = document.createElement('div');
  el.className = 'log-line ' + stream;
  const time = document.createElement('span');
  time.className = 'log-time';
  time.textContent = new Date().toTimeString().slice(0, 8);
  el.appendChild(time);
  el.appendChild(document.createTextNode(line)); // textContent 防注入
  logBody.appendChild(el);
  while (logBody.children.length > 3000) logBody.removeChild(logBody.firstChild);
  if ($('chk-autoscroll').checked) logBody.scrollTop = logBody.scrollHeight;
  // 引导向导打开时，把日志镜像到向导内的输出区（npm 安装输出等）
  const wizLog = $('wiz-log');
  if (!wizLog.classList.contains('hidden')) {
    wizLog.textContent += line + '\n';
    while (wizLog.textContent.split('\n').length > 500) {
      wizLog.textContent = wizLog.textContent.slice(wizLog.textContent.indexOf('\n') + 1);
    }
    wizLog.scrollTop = wizLog.scrollHeight;
  }
  // 更新输出同时镜像到状态区的「更新进度」面板（点击「更新 DSH」确认后可见）
  if (stream === 'update') {
    const updLog = $('update-progress-log');
    if (updLog) {
      updLog.textContent += line + '\n';
      while (updLog.textContent.split('\n').length > 200) {
        updLog.textContent = updLog.textContent.slice(updLog.textContent.indexOf('\n') + 1);
      }
      updLog.scrollTop = updLog.scrollHeight;
    }
  }
}

// ---------- 剪贴板（带降级方案；WebView2 中 clipboard API 偶尔受限） ----------

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (_) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch (_) { /* 忽略 */ }
    ta.remove();
    return ok;
  }
}

// ---------- Toast ----------

let toastTimer = null;
function toast(msg, isError = false) {
  const el = $('toast');
  el.textContent = msg;
  el.className = 'toast' + (isError ? ' error' : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.add('hidden'), isError ? 6000 : 3500);
}

// ---------- 刷新页面（只刷新内嵌 DSH 页面，不重启服务） ----------

let refreshTimer = null; // 刷新提示层的隐藏定时器

function refreshPage() {
  if (!['running', 'running-external'].includes(status)) {
    toast(t('err_not_running'), true);
    return;
  }
  const ov = $('refresh-overlay');
  ov.classList.remove('hidden');
  clearTimeout(refreshTimer);

  // 内嵌的 DSH 页面是盖在本页面之上的原生 webview，
  // 刷新期间先把它隐藏，才能让本页面的"正在刷新..."提示显示出来。
  invoke('set_dsh_webview_visible', { visible: false }).catch(() => {});

  // 新页面出现后（重新显示 DSH webview）即收起提示层；
  // 这里先按 1.6 秒兜底，避免长时间无响应。
  refreshTimer = setTimeout(() => {
    ov.classList.add('hidden');
    showDshWebviewUnlessModal();
  }, 1600);

  invoke('refresh_dsh_page')
    .then(() => {
      // 页面正在重新加载；提示层由上面的定时器收起
    })
    .catch((err) => {
      clearTimeout(refreshTimer);
      ov.classList.add('hidden');
      showDshWebviewUnlessModal();
      toast(String(err), true);
    });
}

/// 强制要求「显示内嵌页」（刷新流程用），但弹窗还开着时保持隐藏。
/// 顺手把记下的意图同步成实际发出的值 —— 否则去重逻辑会以为后端还是旧状态。
function showDshWebviewUnlessModal() {
  if (anyModalOpen()) return;
  webviewVisibleSent = true;
  invoke('set_dsh_webview_visible', { visible: true }).catch(() => {
    webviewVisibleSent = null;
  });
}

// F5 / Ctrl+R 快捷键：焦点在 Launcher 界面时触发刷新；
// DSH 页面内部 WebView2 自带 F5/Ctrl+R 刷新行为，两者效果一致。
window.addEventListener('keydown', (e) => {
  if (e.key === 'F5' || ((e.ctrlKey || e.metaKey) && (e.key === 'r' || e.key === 'R'))) {
    e.preventDefault();
    refreshPage();
    return;
  }
  // Ctrl+Shift+H：固定显示 ⇄ 自动隐藏（与浏览器/系统快捷键不冲突）。
  // 只有焦点在本页面上时才收得到 —— 焦点在内嵌 DSH 页面里时按键归它，
  // 那种情况下用鼠标到窗口顶部触发即可（见 README 的说明）。
  if (e.ctrlKey && e.shiftKey && !e.altKey && (e.key === 'H' || e.key === 'h')) {
    e.preventDefault();
    toggleToolbarMode();
  }
});

// ---------- 模态框（打开时隐藏内嵌 DSH webview，避免其盖住弹窗） ----------

const MODALS = ['settings-modal', 'log-modal', 'update-modal', 'safe-modal', 'safe-verify-modal', 'uninstall-modal', 'uninstall-pnpm-modal'];

function anyModalOpen() {
  return MODALS.some((m) => !$(m).classList.contains('hidden'));
}

/// 已经发给后端的内嵌页显隐意图（null = 还没发过）。
///
/// 为什么要记：弹窗（DOM，同步）与内嵌 webview（原生子窗口，**异步 IPC**）是两条独立通道。
/// 一个「打开 A 再打开 B」的动作会连续触发 open→close→open，连发三次意图；只要有一次
/// 顺序颠倒或消息丢失，最终生效的就可能是「显示内嵌页」—— 于是弹窗被原生页盖住，
/// 用户看到的是「工具栏变暗、什么都点不动、日志里一条错都没有」。
/// 记住上次发过的值，只在**真的变化**时才发，这类抖动窗口就从根上少了一大半。
let webviewVisibleSent = null;

function syncWebviewVisibility() {
  const want = !anyModalOpen();
  if (want === webviewVisibleSent) return; // 与后端已达成的状态一致：不必再发
  webviewVisibleSent = want;
  // 无 webview 时 Rust 端为 no-op，可安全调用
  invoke('set_dsh_webview_visible', { visible: want }).catch(() => {
    // 发送失败就把记录清掉，让下一次同步重新尝试（否则会一直以为「已经隐藏了」）
    webviewVisibleSent = null;
  });
}

/// 页面重新就绪 / 状态回到 running 时调用：把「当前是否该显示内嵌页」重新申明一次。
/// 与 syncWebviewVisibility 的区别是**强制重发**——那时内嵌 webview 可能刚被重建
/// （新建的页面默认可见），后端记录的意图与它在 DOM 上的现实未必一致。
function resyncWebviewVisibility() {
  webviewVisibleSent = null;
  syncWebviewVisibility();
}

function showModal(id) {
  $(id).classList.remove('hidden');
  // 弹窗（首选项 / 日志 / 更新 / 安全模式）打开期间禁止工具栏自动收起：
  // 这里的定时器不允许跨弹窗存活，收起判定里还有一道 anyModalOpen() 兜底。
  cancelToolbarHideTimer();
  syncWebviewVisibility();
}

function hideModal(id) {
  $(id).classList.add('hidden');
  syncWebviewVisibility();
  // 弹窗关掉后按正常规则重新计时：鼠标若不在工具栏上，500ms 后自动收起
  scheduleToolbarHide();
}

// ---------- 等待秒表（starting 期间显示已等待秒数） ----------

let waitTimer = null;
let waitStart = 0;

function startWaitTimer() {
  stopWaitTimer();
  waitStart = Date.now();
  renderWaitLine(0);
  waitTimer = setInterval(() => {
    renderWaitLine(Math.floor((Date.now() - waitStart) / 1000));
  }, 1000);
}

function renderWaitLine(secs) {
  const timeout = config ? config.health_timeout_secs : 0;
  const tail = timeout > 0 ? t('wait_tail_timeout', timeout) : t('wait_tail_infinite');
  const prefix = statusMessage ? `${statusMessage} — ` : t('wait_prefix');
  $('stage-line').textContent = t('wait_line', prefix, secs, tail);
}

function stopWaitTimer() {
  if (waitTimer) clearInterval(waitTimer);
  waitTimer = null;
}

// ---------- 启动等待页诗行（每行显示 5 秒，最后一行保留到 DSH 页面就绪） ----------

const POEM_LINE_MS = 5000;
let poemTimer = null;
let poemActive = false;
let poemIndex = -1;

function getPoemLines() {
  const dict = I18N.dict[I18N.lang] || I18N.dict.zh;
  return dict.poem_lines || [];
}

function showPoemLine() {
  const lines = getPoemLines();
  if (!lines.length || poemIndex < 0 || poemIndex >= lines.length) return;
  $('poem-line').textContent = lines[poemIndex];
  // 出处只显示在最后一行（不限时、一直保留到就绪）的行末下方
  const src = $('poem-source');
  if (poemIndex === lines.length - 1) {
    src.innerHTML = t('poem_source');
  } else {
    src.innerHTML = '';
  }
}

function scheduleNextPoemLine() {
  clearTimeout(poemTimer);
  const lines = getPoemLines();
  // 最后一行不限时，一直保留到 DSH 页面就绪
  if (!poemActive || poemIndex >= lines.length - 1) {
    poemTimer = null;
    return;
  }
  poemTimer = setTimeout(() => {
    poemIndex++;
    showPoemLine();
    scheduleNextPoemLine();
  }, POEM_LINE_MS);
}

function startPoem() {
  stopPoem();
  const lines = getPoemLines();
  if (!lines.length) return;
  poemActive = true;
  poemIndex = 0;
  $('poem-panel').classList.remove('hidden');
  showPoemLine();
  scheduleNextPoemLine();
}

function stopPoem() {
  poemActive = false;
  clearTimeout(poemTimer);
  poemTimer = null;
  poemIndex = -1;
  $('poem-panel').classList.add('hidden');
  $('poem-line').textContent = '';
  $('poem-source').innerHTML = '';
}

// ---------- 状态 UI ----------

function onStatus(p) {
  const prev = status;
  status = p.status;
  // 防御性同步：状态事件说处于安全模式而本地标记未置（事件丢失时兜底），
  // 从后端补一次完整的 SafeReport 再渲染。
  //
  // 注意顺序：这里要**先落琥珀标记再继续往下**（同步 applySafeUI），不能只靠
  // `invoke('get_safe_status').then(...)` 那一路 —— 那是异步的，会晚于本次状态渲染。
  // 竞态就在这段时间里：安全实例就绪时 Rust 先 `set_status("running")` 再
  // `open_dsh_webview`（见 process.rs wait_ready_and_embed），于是本函数会带着
  // `safeMode` 仍为 false 一路跑到 `refreshButtons()` —— 而 refreshButtons 此时已经
  // 从 `starting` 里出来了，`busy` 变 false，启动/停止/重启/更新四个按钮**不是禁用态**
  // 而是"可点但没有任何反应"（Rust 侧有门禁，点了没动静）。更关键的是
  // `applyToolbarMode()` 会在 `safeMode === false` 下算出 `auto = true`，给 body 挂上
  // `.tb-auto` —— 工具栏立刻按自动隐藏规则滑走，而 `#tb-band` 还是**日常模式的浅灰**，
  // 于是用户看到的就是"刚进安全模式时工具栏是暗的"，鼠标点一下触发焦点重排才恢复。
  // 先同步置标记，后面所有判定（refreshButtons / applyToolbarMode）当场就是对的。
  if (p.safe_mode && !safeMode && !safeBusy) {
    applySafeUI(true, safeReport || { port: p.port });
  }
  statusMessage = p.message || null; // 供 renderWaitLine 显示开机自启延迟等提示（Rust 端已本地化）
  const map = STATUS_META[p.status] || { key: null, dot: 'gray' };
  $('status-dot').className = 'dot ' + map.dot;
  $('status-text').textContent = map.key ? t(map.key) : p.status;
  $('port-val').textContent = p.port;

  // 内嵌 DSH 页面是「就绪后才创建」的原生 webview（盖在本页面之上）：
  // 打开模态框那一刻调用的 set_dsh_webview_visible(false) 对还不存在的 webview 是空操作，
  // 于是弹窗会被刚创建的页面盖住（安全模式引导横幅正好在就绪前后显示，最易撞上；
  // 日常模式在启动中打开设置/日志时同理）。就绪时机**强制重发**一次意图（resync：
  // 页面可能刚重建、后端记录与现实未必一致），并留一次延迟兜底。
  if (p.status === 'running' || p.status === 'running-external') {
    resyncWebviewVisibility();
    setTimeout(resyncWebviewVisibility, 400);
  }

  const line = $('stage-line');
  const hint = $('stage-hint');
  const busyPanel = $('port-busy-panel');
  const showSpinner = ['starting', 'stopping', 'updating'].includes(p.status);

  $('spinner').classList.toggle('hidden', !showSpinner);
  line.classList.remove('error');
  hint.classList.add('hidden');
  busyPanel.classList.add('hidden');
  $('btn-copy-error').classList.add('hidden');
  // 更新进度面板：进入 updating 状态（或更新流程尚未结束，覆盖「停止→更新」的中间态）时显示
  $('update-progress').classList.toggle('hidden', !(p.status === 'updating' || updating));

  switch (p.status) {
    case 'idle':
      line.textContent = t('stage_idle');
      break;
    case 'starting':
      startWaitTimer(); // 内部会渲染等待行
      // 启动等待页显示诗行；语言切换后仍停留在启动页时按新词典更新当前行，不重头开始
      if (poemActive) {
        $('poem-panel').classList.remove('hidden');
        showPoemLine();
      } else {
        startPoem();
      }
      hint.textContent = t('hint_starting');
      hint.classList.remove('hidden');
      break;
    case 'running':
      line.textContent = t('stage_running');
      break;
    case 'running-external':
      line.textContent = t('stage_running_ext');
      break;
    case 'stopping':
      line.textContent = t('stage_stopping');
      break;
    case 'updating':
      line.textContent = t('stage_updating');
      hint.textContent = t('hint_updating');
      hint.classList.remove('hidden');
      break;
    case 'error':
      line.textContent = p.message || t('stage_error_default');
      line.classList.add('error');
      hint.textContent = t('hint_error');
      hint.classList.remove('hidden');
      lastErrorText = line.textContent;
      $('btn-copy-error').classList.remove('hidden');
      break;
    case 'port-busy':
      line.textContent = t('stage_port_busy', p.port);
      busyPanel.classList.remove('hidden');
      $('busy-port').textContent = p.port;
      lastErrorText = p.message || line.textContent;
      $('btn-copy-error').classList.remove('hidden');
      break;
  }

  if (p.status !== 'starting') {
    stopWaitTimer();
    stopPoem(); // 离开启动页即隐藏诗行（含“DSH 已就绪，页面即将显示”）
  }

  // DSH 服务启动/重启完成（进入运行态）时，自动刷新右上角的版本显示
  if (
    (p.status === 'running' || p.status === 'running-external') &&
    prev !== 'running' && prev !== 'running-external'
  ) {
    autoCheckVersions();
  }

  // 工具栏显示模式重新结算：它依赖「DSH 页面是否已就绪」（见 syncToolbarForStatus）
  syncToolbarForStatus();

  refreshButtons();
}

function refreshButtons() {
  const busy = updating || ['starting', 'stopping', 'updating'].includes(status);
  // 安全模式（或进入/退出流程进行中）：日常控制全部禁用——状态机归安全实例所有
  // （Rust 侧同样有门禁，这里只是让按钮如实反映）；保留「退出安全模式」、
  // 日志与首选项。
  if (safeMode || safeBusy) {
    $('btn-start').disabled = true;
    $('btn-stop').disabled = true;
    $('btn-restart').disabled = true;
    $('btn-update').disabled = true;
    $('btn-connect').disabled = true;
    $('btn-safe').disabled = busy || safeBusy;
    $('btn-settings').disabled = updating;
    // 安全模式期间后端会拒绝卸载（err_safe_active_op），界面也如实禁用
    const unSafe = $('btn-uninstall-dsh');
    if (unSafe) unSafe.disabled = true;
    return;
  }
  $('btn-start').disabled = busy || ['running', 'running-external'].includes(status);
  $('btn-stop').disabled = updating || ['idle', 'error', 'port-busy', 'stopping'].includes(status);
  $('btn-restart').disabled = busy || !['running', 'running-external'].includes(status);
  $('btn-update').disabled = busy || !config || !config.npm_exists;
  $('btn-safe').disabled = busy;
  $('btn-settings').disabled = updating;
  $('btn-connect').disabled = updating;
  // 「卸载 DSH」只在确实检测到 DSH 时可点（没装就没什么可卸的）
  const un = $('btn-uninstall-dsh');
  if (un) un.disabled = updating || !config || !config.dsh_exists;
}

// ---------- 安全模式 UI（独立纯净家目录 + 端口 3081；流程编排在 Rust 侧 safe.rs） ----------

/// 工具栏按钮文字：未进入时「安全模式」，进入后「退出安全模式」。
/// 动态文案，刻意不挂 data-i18n（避免 applyDom 用词典覆盖），
/// 语言切换与安全模式进出时都要重绘。
function renderSafeButton() {
  $('btn-safe').textContent = safeMode ? t('btn_exit_safe_mode') : t('btn_safe_mode');
}

/// 渲染引导横幅：产品要求明确告知的三件事（处于安全模式 / 日常家目录路径 /
/// 凭据借用结果）+ 归档信息。credential_message 由 Rust 端按界面语言生成，
/// 前端原样显示——凭据文件内容全程不经过 IPC。
function fillSafeModal(report) {
  $('safe-fact-running').innerHTML = t('safe_fact_running_html', report ? report.port : 3081);
  $('safe-daily-home').textContent = (report && report.daily_home) || (config && config.dsh_home_dir) || '—';
  $('safe-home').textContent = (report && report.safe_home) || '—';
  const archLine = $('safe-archive-line');
  if (report && report.archived_to) {
    $('safe-archived-to').textContent = report.archived_to;
    archLine.classList.remove('hidden');
  } else {
    archLine.classList.add('hidden');
  }
  const cred = $('safe-cred-line');
  const borrowed = !!(report && report.credential === 'borrowed');
  cred.className = 'safe-cred' + (borrowed ? ' ok' : '');
  cred.textContent = (borrowed ? '✔ ' : '⚠ ') + ((report && report.credential_message) || '');
}

/// 进出安全模式：切换工具栏琥珀标记（body.safe-mode）、徽标、按钮文字与可用性策略。
/// 窗口标题的「[安全模式]」前缀由 Rust 侧同步设置（apply_safe_window_title）。
function applySafeUI(active, report) {
  safeMode = active;
  safeReport = active ? (report || safeReport) : null;
  document.body.classList.toggle('safe-mode', active);
  $('safe-badge').classList.toggle('hidden', !active);
  renderSafeButton();
  if (active) fillSafeModal(safeReport);
  // 安全模式的工具栏恒为固定显示（不可更改）：选项当场禁用，但勾选状态照旧反映
  // 用户已保存的偏好 —— 保存时也按偏好写回，不会因为"在安全模式里点了一次保存"被改掉。
  const toolbarBox = $('set-toolbar-auto');
  if (toolbarBox) {
    toolbarBox.disabled = active;
    toolbarBox.checked = toolbarPrefAuto();
  }
  applyToolbarMode();
  refreshButtons();
}

function onSafeModeChange(p) {
  safeBusy = false;
  if (p.active) {
    applySafeUI(true, p.report);
    toast(t('toast_safe_entered', p.report ? p.report.port : 3081));
    // 进入成功：弹出引导横幅（safe-modal 在 MODALS 里，打开期间自动隐藏内嵌 webview）
    showModal('safe-modal');
  } else {
    applySafeUI(false);
    if (p.phase === 'exited') {
      toast(t('toast_safe_exiting'));
    } else if (p.phase === 'crashed') {
      toast(t('toast_safe_crashed', p.message || ''), true);
    } else if (p.phase === 'timeout') {
      toast(p.message || '', true); // 详情同时显示在状态区（与日常启动超时同款处理）
    } else if (p.message) {
      toast(t('toast_safe_enter_fail', p.message), true);
    }
  }
  refreshButtons();
  // 工具栏显示模式：进入/退出安全模式都会翻转 `toolbarAutoActive()`，必须重算一次 ——
  // 但要**排在 refreshButtons 之后**，因为 applyToolbarMode 会把 `toolbarLive` 刷成
  // 当前 `dshPageLive()`。本事件到达时安全实例通常还在 `starting`（Rust 是先 spawn +
  // set_status("starting")，等 HTTP 就绪才 emit_safe_change("entered")），
  // 此刻 toolbarLive 应当是 false；等真正 running 时 onStatus → syncToolbarForStatus
  // 会再算一次并把它纠正过来。顺序反过来会让状态与标记短暂不一致。
  applyToolbarMode();
}

/// 修复验证闭环：退出安全模式后 Rust 端监控日常实例就绪情况，超时/失败发 safe-verify
function onSafeVerify(p) {
  if (p.success) {
    appendLog('launcher', t('log_safe_verify_ok'));
    toast(t('toast_safe_verify_ok'));
  } else {
    $('safe-verify-msg').textContent = p.message || '';
    showModal('safe-verify-modal');
  }
}

async function enterSafeMode() {
  if (safeBusy || safeMode) return;
  safeBusy = true;
  refreshButtons();
  appendLog('launcher', t('log_safe_ui_enter'));
  toast(t('toast_safe_entering'));
  try {
    await invoke('enter_safe_mode');
    // 过程与结果由 dsh-status / dsh-log / safe-mode-change 事件驱动
  } catch (e) {
    safeBusy = false;
    refreshButtons();
    toast(String(e), true);
  }
}

async function exitSafeMode() {
  if (safeBusy || !safeMode) return;
  hideModal('safe-modal');
  safeBusy = true;
  refreshButtons();
  appendLog('launcher', t('log_safe_ui_exit'));
  try {
    await invoke('exit_safe_mode');
  } catch (e) {
    safeBusy = false;
    refreshButtons();
    toast(t('toast_safe_exit_fail', e), true);
  }
}
// ---------- 工具栏模式（固定显示 / 自动隐藏） ----------
//
// 布局真相：工具栏在本页面（launcher 的 label="main" webview）里，而内容区是一个
// **原生子 webview**（label="dsh"），它盖在本页面之上，位置与大小只能由 Rust 改。
// 所以自动隐藏是前后端协作的：
//   - 前端：工具栏 translateY(-100%) 滑出 / translateY(0) 滑入（CSS transform，GPU 加速），
//     负责悬停判定、500ms 延迟、弹窗期间禁止收起，并把「收起」状态上报给后端；
//   - 后端：把内嵌页面按当前状态摆好 —— 收起时顶到 y=0 铺满整窗（内容真的占满窗口），
//     展开 / 固定显示时下移一个工具栏高度（process.rs 的 content_offset_for）。
// 另一件事只能由后端做：收起时那条透明触发条被原生子 webview 盖住，本页面收不到任何
// 鼠标事件，所以「鼠标回到窗口顶部」要由外壳读光标位置后通知（probe_toolbar_hotzone）。
//
// 生效范围：只有「日常模式 + DSH 页面已就绪」才自动隐藏 ——
//   * 安全模式强制固定显示（退出入口就长在工具栏上，藏起来等于把用户困住）；
//   * DSH 没起来时状态区就是全部内容，工具栏必须留着（启动/停止/重试都靠它）。

const TOOLBAR_HOT_ZONE_PX = 8;   // 与 style.css 的 #tb-hotzone 高度、Rust 的 TOOLBAR_HOT_ZONE 一致
const TOOLBAR_ANIM_MS = 260;     // 与 CSS 的 transition 0.25s 对应（留一点余量）
const AUTO_HIDE_DELAY_MS = 500;  // 鼠标离开工具栏后的收起延迟
const HOT_PROBE_MS = 120;        // 收起状态下探测光标「是否回到顶部」的间隔

let toolbarLive = false;      // 内嵌 DSH 页面是否已就绪（决定自动隐藏有没有意义）
let toolbarHovered = false;   // 鼠标是否停在工具栏上
let toolbarHideTimer = null;  // 离开工具栏后的收起倒计时
let toolbarSettleTimer = null;// 滑出动画结束后「把内容区扩到整窗」的定时器
let toolbarProbeTimer = null; // 收起状态下的光标探测定时器
let toolbarProbeBusy = false; // 上一次探测还没回来时跳过本轮（避免 IPC 堆积）

function toolbarPrefAuto() { return !!(config && config.toolbar_mode === 'auto'); }
function dshPageLive() { return status === 'running' || status === 'running-external'; }
/// 自动隐藏此刻是否真的生效（安全模式 / 未选自动隐藏 / DSH 页面未就绪 都不生效）
function toolbarAutoActive() { return toolbarPrefAuto() && !safeMode && toolbarLive; }

/// 把「收起」状态同步给后端：内嵌页面是原生子 webview，只有 Rust 能改它的位置 ——
/// 收起时顶到 y=0 铺满整窗，展开时下移一个工具栏高度。
/// 返回值是后端**实际生效**的收起状态（安全模式会拒绝收起），前端据此纠偏，
/// 免得两边状态不一致：界面以为收起了、后端却让内容铺满整窗，把工具栏和入口一起盖住。
function setToolbarHidden(hidden) {
  invoke('set_toolbar_hidden', { hidden: !!hidden })
    .then((effective) => {
      if (hidden && effective === false) document.body.classList.add('tb-shown');
    })
    .catch(() => {});
}

function stopToolbarProbe() {
  if (toolbarProbeTimer) {
    clearInterval(toolbarProbeTimer);
    toolbarProbeTimer = null;
  }
}

/// 收起状态下开始探测「鼠标是否回到窗口顶部」。
/// 不能在页面里直接听 mousemove：收起时原生子 webview 铺满整窗，
/// 本页面（连同那条透明触发条）收不到任何鼠标事件。
function startToolbarProbe() {
  if (toolbarProbeTimer) return;
  toolbarProbeTimer = setInterval(() => {
    // 模式已经不生效（切回固定显示 / 进了安全模式 / DSH 页面没了）：自己停掉，
    // 免得留下一个每 120ms 空转的定时器
    if (!toolbarAutoActive()) { stopToolbarProbe(); return; }
    if (document.hidden || toolbarProbeBusy) return;
    toolbarProbeBusy = true;
    invoke('probe_toolbar_hotzone')
      .then((hot) => { if (hot) showToolbar(); })
      .catch(() => {})
      .then(() => { toolbarProbeBusy = false; });
  }, HOT_PROBE_MS);
}

function cancelToolbarHideTimer() {
  if (toolbarHideTimer) { clearTimeout(toolbarHideTimer); toolbarHideTimer = null; }
}

function cancelToolbarSettleTimer() {
  if (toolbarSettleTimer) { clearTimeout(toolbarSettleTimer); toolbarSettleTimer = null; }
}

/// 展开工具栏（鼠标进入触发条 / 工具栏，或后端探测到光标靠近顶部时调用）
function showToolbar() {
  if (!toolbarAutoActive()) return;
  stopToolbarProbe();            // 已经展开了，探测器先停（收起时再开）
  cancelToolbarHideTimer();
  cancelToolbarSettleTimer();
  if (document.body.classList.contains('tb-shown')) return;
  document.body.classList.add('tb-shown');
  setToolbarHidden(false);
}

/// 收起工具栏（两段式）：先让 CSS 把工具栏滑上去（此时内嵌页面还在下移后的位置，
/// 露出的那条是 #tb-band），动画结束后再让内容区扩到整窗。
/// 中间若鼠标回到热区（showToolbar 会取消 settle 定时器），不会出现「刚滑出来又被盖住」。
function hideToolbar() {
  if (!toolbarAutoActive() || toolbarHovered || anyModalOpen()) return;
  if (!document.body.classList.contains('tb-shown')) {
    startToolbarProbe();         // 已经是收起态：保证探测器在跑
    return;
  }
  document.body.classList.remove('tb-shown');
  cancelToolbarSettleTimer();
  toolbarSettleTimer = setTimeout(() => {
    toolbarSettleTimer = null;
    if (!toolbarAutoActive() || toolbarHovered) return;
    setToolbarHidden(true);
    startToolbarProbe();
  }, TOOLBAR_ANIM_MS);
}

/// 鼠标离开工具栏后启动延迟收起（延迟期间回到工具栏会被 showToolbar 取消）
function scheduleToolbarHide() {
  if (!toolbarAutoActive()) return;
  cancelToolbarHideTimer();
  toolbarHideTimer = setTimeout(() => {
    toolbarHideTimer = null;
    hideToolbar();
  }, AUTO_HIDE_DELAY_MS);
}

/// 应用工具栏模式：固定显示 / 安全模式 / DSH 页面就绪状态变化时都会调用。
/// 自动隐藏时先展开一次（鼠标不在工具栏上就由 500ms 延迟自然收起），
/// 免得刚切过来、鼠标还在别处时整条工具栏凭空消失。
function applyToolbarMode() {
  toolbarLive = dshPageLive();
  const auto = toolbarAutoActive();
  document.body.classList.toggle('tb-auto', auto);
  cancelToolbarHideTimer();
  cancelToolbarSettleTimer();
  if (!auto) {
    document.body.classList.add('tb-shown');  // 固定显示：始终展开（该类只在 .tb-auto 下有样式）
    stopToolbarProbe();
    // 安全模式下必须显式把「收起」状态清干净：`auto` 为 false 只说明**本函数这次**
    // 不主动收起，但如果上一刻还挂着 `.tb-auto`（例如刚进入安全模式的那一瞬间），
    // 工具栏在视觉上就是"滑走上去了"，而 #tb-band 还留着日常模式的浅灰 ——
    // 表现正是"刚进安全模式时工具栏是暗的，点一下才亮"。这里连同类名一起复位。
    document.body.classList.remove('tb-hover');
    setToolbarHidden(false);
    return;
  }
  if (!document.body.classList.contains('tb-shown')) {
    document.body.classList.add('tb-shown');
    setToolbarHidden(false);
  }
  if (!toolbarHovered) scheduleToolbarHide();
}

/// DSH 页面就绪状态**变化**时才重算（普通状态事件不该重置收起的倒计时）
function syncToolbarForStatus() {
  if (dshPageLive() === toolbarLive) return;
  applyToolbarMode();
}

/// 快捷键 Ctrl+Shift+H：在「固定显示 / 自动隐藏」之间快速切换，并把偏好写回配置。
/// 焦点必须在本页面（Launcher）上；焦点在内嵌 DSH 页面里时按键归它处理（见 README）。
async function toggleToolbarMode() {
  if (safeMode) {
    toast(t('toast_toolbar_safe_locked'), true);
    return;
  }
  const next = toolbarPrefAuto() ? 'pinned' : 'auto';
  try {
    const mode = await invoke('set_toolbar_mode', { mode: next });
    if (config) config.toolbar_mode = mode;
    const box = $('set-toolbar-auto');
    if (box) box.checked = mode === 'auto';
    applyToolbarMode();
    toast(mode === 'auto' ? t('toast_toolbar_auto_on') : t('toast_toolbar_auto_off'));
  } catch (e) {
    toast(String(e), true);
  }
}

// ---------- 事件监听 + 初始化 ----------

async function init() {
  await listen('dsh-log', (e) => appendLog(e.payload.stream, e.payload.line));
  await listen('update-log', (e) => appendLog('update', e.payload.line));
  await listen('update-progress', (e) => onUpdateProgress(e.payload));
  await listen('dsh-status', (e) => onStatus(e.payload));
  await listen('update-finished', (e) => onUpdateFinished(e.payload));
  await listen('path-picked', (e) => onPathPicked(e.payload));
  await listen('autostart-changed', (e) => {
    // 托盘菜单或设置开关切换了开机自启后同步 UI（line: "on" | "off"）
    $('set-autostart').checked = e.payload.line === 'on';
  });
  await listen('setup-status', (e) => onSetupStatus(e.payload));
  await listen('setup-result', (e) => onSetupResult(e.payload));
  // 首选项「Python 安装」的逐行输出（pip 的实时输出；由后端 spawn_log_reader 转发）
  await listen('python-log', (e) => appendPythonLog(e.payload.line));
  // 安全模式：进入/退出/闪退（safe-mode-change）与修复验证结果（safe-verify）
  await listen('safe-mode-change', (e) => onSafeModeChange(e.payload));
  await listen('safe-verify', (e) => onSafeVerify(e.payload));

  await refreshConfig();
  // 应用外观（浅色/深色/跟随系统；theme-boot.js 已按缓存预设过，这里以配置为准纠正）
  applyAppearance(config && config.appearance);
  // 应用界面语言（中英文），随后渲染的静态文案全部走词典
  I18N.setLang(config && config.language);
  I18N.applyDom();
  // 安全模式按钮是动态文案（不挂 data-i18n），需要单独渲染一次
  renderSafeButton();

  const st = await invoke('get_status');
  onStatus(st);

  // 恢复安全模式 UI（页面重建/从托盘恢复等场景）：激活则应用标记色与徽标，
  // 引导横幅不重弹（只在真正进入的那一刻弹一次）
  try {
    const s = await invoke('get_safe_status');
    if (s && s.active) applySafeUI(true, s.report);
  } catch (_) { /* 查询失败按非安全模式处理 */ }

  // 工具栏显示模式（固定 / 自动隐藏）：必须赶在 DSH 启动、内嵌页面被创建之前生效，
  // 否则那个原生子 webview 会先按固定模式摆好位置、再被纠正一次（看得见的跳动）
  applyToolbarMode();

  bindUI();

  // 从托盘恢复窗口时刷新状态，并确保内嵌 DSH Webview 重新显示
  // （WebView2 在 hide→show 后偶发白屏，这里再触发一次重绘兜底）
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) {
      syncWebviewVisibility();
      invoke('get_status').then(onStatus).catch(() => {});
      // 窗口从托盘恢复：悬停状态可能在隐藏期间失真（mouseleave 收不到），
      // 这里重置一次并按正常规则重新计时，免得工具栏卡在"以为鼠标还在上面"
      toolbarHovered = false;
      scheduleToolbarHide();
    } else {
      // 隐藏到托盘：清掉悬停与倒计时，恢复时重新判定
      toolbarHovered = false;
      cancelToolbarHideTimer();
    }
  });

  // 首次运行（配置文件尚不存在）：先走环境检查 / 引导安装向导
  if (config && config.first_run) {
    await runSetupWizard();
    return;
  }

  await postInit();
}

/// 向导结束（完成/跳过）后与老用户相同的初始化尾部
async function postInit() {
  await refreshAutostartToggle();

  // 本次是否由「开机自启」触发（决定静默窗口 + 12 秒延迟启动）
  try {
    launchedByAutostart = await invoke('was_launched_by_autostart');
  } catch (_) { /* 保持 false */ }
  if (launchedByAutostart) {
    appendLog('launcher', t('log_autostart_silent'));
  }

  // 自动启动：DSH 路径有效时，程序启动即拉起服务（含上次超时失败的 error 状态）；
  // 若为开机自启，Rust 端 start_internal 会先延迟 12 秒
  if (config && config.dsh_exists && ['idle', 'error'].includes(status)) {
    appendLog('launcher', t('log_prog_started'));
    try {
      await invoke('start_dsh');
    } catch (err) {
      appendLog('launcher', t('log_auto_start_fail', err));
    }
  } else if (config && !config.dsh_exists) {
    appendLog(
      'launcher',
      t('log_dsh_missing', config.dsh_path || t('wiz_notfound'),
        t('msg_install_dsh', config.package_name || '@deepseek-ai/dsh')),
    );
    showModal('settings-modal');
  }

  // 程序启动/重启时自动检查 DSH 版本，结果显示在右上角状态栏
  // （原工具栏「检查版本」按钮已移除；服务随后进入运行态时 onStatus 还会再触发一次，
  //   由 autoCheckVersions 的 60 秒冷却保证不会重复请求 npm registry）。
  // 开机自启时同样要错开系统冷启动高峰（与 Rust 端「DSH 延迟 12 秒启动」同一考量）：
  // 这里把版本查询推迟 30 秒，避免开机瞬间和系统抢 CPU / 网络。
  if (launchedByAutostart) {
    setTimeout(() => autoCheckVersions(), 30_000);
  } else {
    autoCheckVersions();
  }
}

// 拉取系统层面的开机自启注册状态并同步到设置开关
async function refreshAutostartToggle() {
  try {
    $('set-autostart').checked = await invoke('is_autostart_enabled');
  } catch (_) { /* 查询失败保持原样 */ }
}

async function refreshConfig() {
  config = await invoke('get_config');
  $('port-val').textContent = config.port;
}

// ---------- 外观（浅色 / 深色 / 跟随系统；与 DSH 的 ui-theme.preference 同源联动） ----------
// data-theme 只承载解析后的 light/dark 两个值：「跟随系统」由 prefers-color-scheme 解析，
// 并订阅系统深浅反转实时重解析（仅当配置为 system 时生效）。
// 解析结果缓存进 localStorage，供 theme-boot.js 在下次启动首帧前预设，防「先白一下」；
// 真相来源始终是 config.appearance（localStorage 只是防闪缓存）。
// DSH 页面一侧由 Rust 后端把同一值写入 settings.yaml（ui-theme.preference），实时跟随。
const darkMq = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;

function resolveDark(pref) {
  if (pref === 'dark') return true;
  if (pref === 'light') return false;
  return !!(darkMq && darkMq.matches); // system（含非法值的兜底）
}

function applyAppearance(pref) {
  const p = (pref === 'light' || pref === 'dark') ? pref : 'system';
  const dark = resolveDark(p);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  try { localStorage.setItem('dsh-desktop-theme', dark ? 'dark' : 'light'); } catch (_) { /* 忽略 */ }
}

if (darkMq) {
  const onSchemeFlip = () => {
    if (config && config.appearance === 'system') applyAppearance('system');
  };
  // Safari<14 只有 addListener；WebView2 支持 addEventListener，双保险
  if (darkMq.addEventListener) darkMq.addEventListener('change', onSchemeFlip);
  else if (darkMq.addListener) darkMq.addListener(onSchemeFlip);
}

// ---------- 语言切换（保存后立即应用；重启 Desktop 同样生效） ----------

function applyLanguage(lang) {
  I18N.setLang(lang);
  I18N.applyDom();
  // applyDom 会把 #ver-val 重置成词典默认值（「未知」），而「可更新」标记没有挂
  // data-i18n（它是动态文案）：这里用缓存的检测结果按新语言重绘，避免切语言后
  // 版本栏退回「未知」或残留旧语言的标记。
  const v = lastVersionInfo || {};
  renderVersion(v.local, v.latest, v.next);
  if (!$('update-modal').classList.contains('hidden')) renderUpdateModal(false);
  invoke('get_status').then(onStatus).catch(() => {});
  // 向导里的 Node 版本文案带版本号（下限 + 本机版本），语言切换后要按新词典重绘
  if (wiz.active) renderWiz();
  // 安全模式按钮文字与引导横幅里的动态行（不受 data-i18n 管理）需要手动重绘
  // （凭据借用说明是进入时由 Rust 生成的，语言切换后要到下次进入才更新）
  renderSafeButton();
  if (safeMode && !$('safe-modal').classList.contains('hidden')) fillSafeModal(safeReport);
  // 首选项里的 Python 状态行是动态文案（版本号 + 路径 + 包状态），切语言后按新词典重绘
  if (!$('settings-modal').classList.contains('hidden')) refreshPythonStatus();
  // 「用户环境变量 DSH_HOME」那行小字同样没挂 data-i18n（值是问后端拿的），切语言后重绘
  if (!$('settings-modal').classList.contains('hidden')) refreshHomeEnvInfo();
  // 搬家警告的显隐按「这一格有没有被改过」判（与语言无关），但正文是 data-i18n-html，
  // 切语言后仍要重判一次：此刻 config 已是新语言的值，未改过的一格不该被误判成改过
  if (!$('settings-modal').classList.contains('hidden')) refreshHomeMoveWarn();
  // 包源 registry 那行由后端按语言拼好，切语言后同样要重绘
  if (!$('settings-modal').classList.contains('hidden')) refreshRegistryInfo();
}

// ---------- 设置 ----------

function openSettings() {
  if (!config) return;
  // npm / dsh 的程序路径不再有输入框（用户不该改动它们，见 index.html 的说明）：
  // 保存时原样带回内存里那份（后端加载时已自动检测/回填），不做任何界面读写。
  $('set-npm-cache').value = config.npm_cache_dir || '';
  $('set-home-dir').value = config.dsh_home_dir;
  $('set-port').value = config.port;
  $('set-timeout').value = config.health_timeout_secs;
  $('set-close-action').value = config.close_action === 'quit' ? 'quit' : 'tray';
  $('set-language').value = config.language === 'en' ? 'en' : 'zh';
  $('set-appearance').value = ['light', 'dark', 'system'].includes(config.appearance) ? config.appearance : 'system';
  $('set-extra-args').value = config.extra_args;
  $('set-package-name').value = config.package_name;
  // 工具栏显示模式：勾选 = 自动隐藏（安全模式下强制固定显示，故当场禁用）
  $('set-toolbar-auto').checked = toolbarPrefAuto();
  $('set-toolbar-auto').disabled = safeMode;
  // 安全模式：基线重置开关（默认关闭：取不到字段也按关闭显示）与修复验证等待秒数（默认 80）
  $('set-safe-reset').checked = config.safe_reset_baseline === true;
  const sv = Number(config.safe_verify_secs);
  $('set-safe-verify').value = Number.isFinite(sv) ? sv : 80;
  $('set-config-path').textContent = config.config_path;
  markFlag('home-exists-flag', config.home_exists);
  // 搬家警告：每次打开设置页按「这一格是不是被改过」重判一次（与浏览/输入事件共用同一条判定）
  refreshHomeMoveWarn();
  // 用户环境变量 DSH_HOME 那行小字（新终端会拿到的持久值）：勾选框已经改成自动规则，
  // 这里只剩「每次打开设置页读一次注册表现值」
  refreshHomeEnvInfo();
  refreshNpmCacheInfo();
  // 包源 registry 那行状态（+ 两个按钮的可见性）：读是零风险的，每次打开都重问一次
  refreshRegistryInfo();
  // Python 块：每次打开都重新问一次后端（安装在后台跑，界面上的状态不能是旧的）
  refreshPythonStatus();
  // 维护块：确认框里那条命令要跟着当前 DSH 实际所在目录走
  refreshUninstallCmd();
  showModal('settings-modal');
}

/// 首选项里「npm 缓存位置」那行小字：同时显示「npm 配置里的值」与「实际生效的值」。
/// 两者不同（被环境变量或项目级 .npmrc 覆盖）时必须都显示 —— 只显示一个，用户会以为
/// 自己填的位置没生效，然后去改一个本来就对的值。
async function refreshNpmCacheInfo() {
  const el = $('npm-cache-effective');
  if (!el) return;
  try {
    const info = await invoke('npm_cache_info');
    const user = (info.user_value || '').trim();
    const eff = (info.effective || '').trim();
    if (!user) {
      el.textContent = t('cache_eff_unset', eff || '?');
    } else if (eff && eff.toLowerCase() !== user.toLowerCase()) {
      el.textContent = t('cache_eff_diff', user, eff);
    } else {
      el.textContent = t('cache_eff_set', user);
    }
  } catch (e) {
    // 问不到（npm 缺失等）就不显示这行：它是提示，不该挡住设置页
    el.textContent = '';
  }
}

function markFlag(id, ok) {
  const el = $(id);
  el.textContent = ok ? t('flag_exists') : t('flag_missing');
  el.className = 'flag ' + (ok ? 'ok' : 'bad');
}

/// 首选项「搬家警告」：**只在「这一格里的家目录与已保存的不同」时露出**。
///
/// 为什么要这个提示（一次真实事故换来的）：家目录里的 `profiles\node_modules` 不是普通
/// 文件，而是几百个指回安装目录的 Windows 目录联接（junction）。常规拷贝工具会把它们
/// 展平成一堆空目录，而 dsh 每次启动都要校验这层安装回退 —— 发现
/// `@deepseek-ai\dsh` 是个真实目录又不受 dsh 托管，就抛错退出，整个服务起不来。
/// 所以「拷贝完删掉 `profiles\node_modules`，让它自己重建」必须写在动手之前。
///
/// 只在这一格被改动时出现：家目录平时纹丝不动，一条常驻的警告等于噪声。
/// 比较用与 Rust 侧同一把尺子（trim + 去尾分隔符 + 小写，见 detect::home_key）——
/// 只差一个大小写或尾斜杠不算「改了」，否则会出现「只是手滑打了个反斜杠就被警告」。
function refreshHomeMoveWarn() {
  const el = $('home-move-warn');
  const input = $('set-home-dir');
  if (!el || !input || !config) return;
  el.classList.toggle('hidden', homeKey(input.value) === homeKey(config.dsh_home_dir || ''));
}

/// 家目录比较键：与 detect::home_key（Rust 侧）同规则——去首尾空白、去尾分隔符、转小写。
function homeKey(s) {
  return String(s == null ? '' : s).trim().replace(/[\\/]+$/, '').toLowerCase();
}

/// 首选项里「用户环境变量 DSH_HOME」那行小字：显示**新终端**会拿到的持久值。
/// 读注册表（dsh_home_env_info）而不是本进程环境 —— 进程环境是启动时的快照，而且本程序
/// 启动 DSH 时还会显式覆盖它；用户关心的是「我自己开的终端里会是什么」。
async function refreshHomeEnvInfo() {
  const el = $('home-env-now');
  if (!el) return;
  try {
    const v = ((await invoke('dsh_home_env_info')) || '').trim();
    el.textContent = v ? t('home_env_now', v) : t('home_env_now_unset');
  } catch (e) {
    // 问不到（注册表被策略锁定等）就不显示：它是提示，不该挡住设置页
    el.textContent = '';
  }
}

/// 首选项里「包源 registry」那行状态：npm 与 pnpm 在 **DSH 工作目录**下各自生效的源。
/// 整行文案由后端按当前语言拼好（同一句话不在两处各维护一份），这里只负责渲染，
/// 外加两个按钮 —— 按钮文案要带目标 URL / 原值，所以用返回的字段自己拼。
///
/// 为什么是按钮而不是开关：改 registry = 改**今后所有安装流量**的去向，属于供应链敏感
/// 操作，而且不是需要持续维护的状态（每次保存都重写会跟用户手动改的值打架）。
/// 所以读（零风险）自动刷，写只在点击时发生。
async function refreshRegistryInfo() {
  const line = $('registry-line');
  const alignBtn = $('btn-registry-align');
  const restoreBtn = $('btn-registry-restore');
  if (!line || !alignBtn || !restoreBtn) return;
  try {
    const info = await invoke('registry_align_info');
    const target = (info.target || '').trim();
    const prev = (info.prev || '').trim();
    line.textContent = info.line || '';
    // 三个条件任一不满足都不显示对齐按钮：没有 pnpm / 已经一致 / 读不到目标
    const showAlign = !!info.pnpm_exists && !info.aligned && !!target;
    alignBtn.classList.toggle('hidden', !showAlign);
    if (showAlign) alignBtn.textContent = t('btn_align_registry', target);
    restoreBtn.classList.toggle('hidden', !prev);
    if (prev) restoreBtn.textContent = t('btn_restore_registry', prev);
  } catch (e) {
    // 问不到（npm / pnpm 缺失等）就整块收起：它是提示，不该挡住设置页
    line.textContent = '';
    alignBtn.classList.add('hidden');
    restoreBtn.classList.add('hidden');
  }
}

// ---------- 首选项：Python 建议安装块 ----------
//
// 与首装向导共用后端的 setup-status / setup-result 事件（文案由 Rust 端按当前语言生成），
// 靠 target = "python" 加这里自己的 busy 标志分流 —— 向导不在这块页面上时，进度不会被
// 拽进向导的进度区，反之亦然。安装跑在后端线程里：用户关掉首选项也没关系，事件照常
// 到达并更新这里的状态，重开首选项会再拉一次 python_status。
//
const pythonTask = { active: false };

/// 最近一次 `python_status` 的快照，只给按钮高亮用。
/// 初始按「什么都没装」处理 —— 与 index.html 里两个按钮的初始 class（基本安装 = 蓝、
/// 数据分析 = 白）一致，这样探测结果没回来之前不会出现高亮来回跳。
const pythonState = { found: false, basic_ok: false, extra_ok: false };

/// 问后端「本机有没有 Python / 什么版本 / 推荐的包装了没」并画状态行。
async function refreshPythonStatus() {
  const el = $('python-status');
  if (!el) return;
  // 安装进行中不去打扰后端（它正忙着），状态行由进度区负责
  if (!pythonTask.active) el.textContent = t('py_checking');
  try {
    renderPythonStatus(await invoke('python_status'));
  } catch (e) {
    // 问不到就如实说一句：状态行只是提示，不该挡住整个设置页。
    // 同时把「安装位置」放开 —— 那一格只在「没装 Python」时露出，而探测失败的机器
    // 十有八九正是没装的那一台；跟着状态一起藏起来，用户就没法预填安装目录了。
    el.textContent = t('toast_py_status_fail', e);
    const dirRow = $('python-dir-row');
    if (dirRow) dirRow.classList.remove('hidden');
  }
}

function renderPythonStatus(s) {
  const el = $('python-status');
  if (!el || !s) return;
  // 快照存下来给按钮高亮用（没找到 Python 时后端必然把两个 ok 置 false，
  // 见 process.rs::python_status_blocking 的提前返回）
  pythonState.found = !!s.found;
  pythonState.basic_ok = !!s.basic_ok;
  pythonState.extra_ok = !!s.extra_ok;
  if (!s.found) {
    el.textContent = t('py_missing');
  } else {
    const ver = s.version ? s.version : t('py_ver_unknown');
    const pkgs = (s.basic_ok && s.extra_ok) ? 'py_pkgs_all'
      : s.basic_ok ? 'py_pkgs_basic_only'
      : s.extra_ok ? 'py_pkgs_extra_only'
      : 'py_pkgs_none';
    // 用中性的「 · 」连接，中英文都不别扭
    el.textContent = t('py_found', ver, s.path || '?') + ' · ' + t(pkgs);
  }
  // 安装位置只在「还没装 Python 本体」时才有意义：装好后藏起来，免得有人以为能改
  const dirRow = $('python-dir-row');
  if (dirRow) dirRow.classList.toggle('hidden', !!s.found);
  // 状态行换字的同时把两个按钮的高亮也刷一遍 —— 蓝色必须跟着「哪一步还没做完」走
  renderPythonButtons();
}

function setPythonProgress(show, text) {
  const el = $('python-progress');
  if (!el) return;
  el.classList.toggle('hidden', !show);
  if (text) $('python-progress-text').textContent = text;
}

/// 进度圈只在任务真正跑着的时候转：结果落地后消息要留着读，圈得收起来。
function setPythonSpinner(show) {
  const sp = document.querySelector('#python-progress .spinner');
  if (sp) sp.classList.toggle('hidden', !show);
}

/// 安装过程的逐行输出（pip 的实时输出走 python-log 事件）。行数封顶：
/// 这块面板是模态框里的一小条，放任增长只会把状态行顶出视野。
function appendPythonLog(line) {
  const pre = $('python-log');
  if (!pre) return;
  pre.classList.remove('hidden');
  pre.textContent += (pre.textContent ? '\n' : '') + line;
  const lines = pre.textContent.split('\n');
  if (lines.length > 300) pre.textContent = lines.slice(-300).join('\n');
  pre.scrollTop = pre.scrollHeight;
}

function renderPythonButtons() {
  // 蓝色（primary）= 当前**该做**的那一步，白色 = 已完成、不需要再点：
  //   基本安装（本体 + 办公文档读写包）没到位 → 基本安装亮蓝，数据分析留白；
  //   基本安装到位 → 基本安装回白，数据分析亮蓝；
  //   两组都到位 → 两个都回白，页面上不再有「还欠一步」的暗示。
  // 数据分析反而先装好的机器也照这条办：欠的是办公包，该亮蓝的仍是基本安装。
  const basicDone = pythonState.basic_ok;
  const extraDone = basicDone && pythonState.extra_ok;
  const setPrimary = (id, on) => {
    const b = $(id);
    if (b) b.classList.toggle('primary', !!on);
  };
  setPrimary('btn-python-basic', !basicDone);
  setPrimary('btn-python-extra', basicDone && !extraDone);
  ['btn-python-basic', 'btn-python-extra'].forEach((id) => {
    const b = $(id);
    if (b) b.disabled = pythonTask.active;
  });
}

/// 两个安装按钮共用的入口：起后端任务，进度与结果由事件驱动。
async function runPythonInstall(set) {
  if (pythonTask.active) {
    toast(t('py_busy'), true);
    return;
  }
  // 基本安装可能连 Python 本体一起装，**开始之前**就把目录交给用户改：
  // 留空 = Python 官方默认位置，填了（或「浏览」选了）后端会先校验再交给安装程序。
  const dirEl = $('set-python-dir');
  const dir = dirEl ? dirEl.value.trim() : '';
  pythonTask.active = true;
  renderPythonButtons();
  $('python-log').textContent = '';
  $('python-log').classList.add('hidden');
  setPythonProgress(true, t('py_progress_start'));
  // 转圈要跟着任务走：结果回来时上面的 onPythonResult 会把它收起来，
  // 否则一条早就结束的「成功」消息旁边还在转圈，看着像卡住了
  setPythonSpinner(true);
  try {
    if (set === 'basic') {
      await invoke('setup_install_python', { dir: dir || null });
    } else {
      await invoke('setup_install_python_extra');
    }
    // 后续由 setup-status / setup-result（target = "python"）事件接手
  } catch (e) {
    // 起不来（比如向导的安装任务占着 busy）：当场把界面复位。
    // 报错文案留在进度区里读，不要把整块藏起来 —— 藏了就只剩一个一闪而过的 toast
    pythonTask.active = false;
    renderPythonButtons();
    setPythonSpinner(false);
    setPythonProgress(true, String(e));
    appendPythonLog(String(e));
    toast(String(e), true);
  }
}

function onPythonResult(p) {
  pythonTask.active = false;
  renderPythonButtons();
  // 结果文案留在进度区（成功与失败都要能读到：toast 只闪几秒）
  const msg = p.message || (p.success ? t('toast_py_ok') : t('toast_py_fail'));
  setPythonProgress(true, msg);
  setPythonSpinner(false);
  appendPythonLog(msg);
  toast(msg, !p.success);
  // 装完 Python 本体 / 装完包之后，状态行与「安装位置」的显隐都要跟着变
  refreshPythonStatus();
}

async function saveSettings() {
  // 端口校验：必须是 1~65535 的数字（要求一.5）
  const port = parseInt($('set-port').value, 10);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    toast(t('toast_port_invalid'), true);
    return;
  }
  const timeout = parseInt($('set-timeout').value, 10);
  const appearance = ['light', 'dark', 'system'].includes($('set-appearance').value)
    ? $('set-appearance').value : 'system';
  const newHome = $('set-home-dir').value.trim();
  // 家目录真要换位置了 —— 点「保存」前把搬家的坑再说一遍（界面上那条警告可能被滚出视野）。
  // 只在**确实变了**时问：没改的一律不问，否则每次存端口都要被无关的警告打断。
  // 留空不算「搬家」（后端 validate_home_dir 会直接报错问不出这个坑），所以不弹这一问。
  if (newHome && homeKey(newHome) !== homeKey((config && config.dsh_home_dir) || '')) {
    const ok = window.confirm(t('confirm_home_change', newHome, newHome + '\\profiles\\node_modules'));
    if (!ok) return;
  }
  const cfg = {
    // npm / dsh 的程序路径没有输入框了（见 index.html 的说明）：原样带回内存里那份，
    // 既不清空也不重新检测 —— 后端保存时只做校验，用户手改过的有效值不会被覆盖。
    // 万一内存里是空的（新装机器还没检测到 npm / DSH），后端会先自动检测再保存。
    npm_path: (config && config.npm_path) || '',
    // npm 缓存位置（空 = 删除 npm 配置里的 cache 行，回到 npm 默认位置）
    npm_cache_dir: $('set-npm-cache').value.trim(),
    dsh_path: (config && config.dsh_path) || '',
    dsh_home_dir: newHome,
    port,
    close_action: $('set-close-action').value === 'quit' ? 'quit' : 'tray',
    language: $('set-language').value === 'en' ? 'en' : 'zh',
    appearance,
    // 工具栏显示模式：安全模式下该选项被禁用，此时按内存里已有的偏好原样带上，
    // 别把"在安全模式里点了一次保存"当成"用户把偏好改成了固定显示"
    toolbar_mode: safeMode
      ? ((config && config.toolbar_mode) || 'pinned')
      : ($('set-toolbar-auto').checked ? 'auto' : 'pinned'),
    // 0 = 一直等待，是合法值，不能用 || 兜底
    health_timeout_secs: Number.isFinite(timeout) && timeout >= 0 ? timeout : 300,
    extra_args: $('set-extra-args').value.trim(),
    package_name: $('set-package-name').value.trim() || '@deepseek-ai/dsh',
    // 安全模式：基线重置开关 + 修复验证等待（0 = 不验证；后端保存时还会收敛范围）
    safe_reset_baseline: $('set-safe-reset').checked,
    safe_verify_secs: (() => {
      const v = parseInt($('set-safe-verify').value, 10);
      return Number.isFinite(v) && v >= 0 ? v : 80;
    })(),
    // 「Node 版本过低」的确认记忆不是设置页字段：原样带上内存里的值，
    // 后端在保存时也会再兜一层（缺字段时沿用磁盘上的值）
    node_min_ack: (config && config.node_min_ack) || '',
    // pnpm 源「对齐前的原值」同样不是设置页字段：它是恢复按钮的原料，**必须原样带回** ——
    // 漏带一次，serde(default) 会把它写成空串，恢复按钮就永久消失了
    pnpm_registry_prev: (config && config.pnpm_registry_prev) || '',
    // 卸载 DSH 时「把 ~/.npmrc 的 prefix= 原路退回」所需的两份记忆，同样不是设置页字段：
    // 漏带一次，卸载就只能走保守路径（保留那一行），用户得到的就不再是「删干净」。
    npm_prefix_prev: (config && config.npm_prefix_prev) || '',
    npm_prefix_claimed: !!(config && config.npm_prefix_claimed),
    // 更新命令不再有可配置参数：固定 install -g <包名>@<频道>，
    // 频道由「更新 DSH」弹窗里的 latest / next 单选决定（见 renderUpdateModal）
  };
  // 本次保存**之前**的家目录：家目录被改回默认值（于是自动删 DSH_HOME）时，
  // 后端要靠它认领上一次写进去的值（必须在 config 被 save_config 的返回值覆盖之前记下来）
  const prevHome = (config && config.dsh_home_dir) || '';
  const langChanged = cfg.language !== I18N.lang;
  const appearanceChanged = appearance !== (config && config.appearance);
  try {
    config = await invoke('save_config', { config: cfg });
    if (appearanceChanged) {
      // 桌面端即时换肤；DSH 页面由后端写入 settings.yaml 后经文件监视实时跟随
      applyAppearance(cfg.appearance);
    }
    if (langChanged) {
      // 先切语言再刷新弹窗内文案（词典 + 静态标签）
      applyLanguage(cfg.language);
    }
    markFlag('home-exists-flag', config.home_exists);
    // 保存后这一格已与 config 一致，搬家警告随之收起（config 在上面刚被返回值覆盖）
    refreshHomeMoveWarn();
    $('set-config-path').textContent = config.config_path;
    $('port-val').textContent = config.port;
    toast(t('toast_saved'));
    // 「npm 缓存位置」还要落到 npm **自己**的配置里（这样终端里的 npm 也跟着用），
    // 这一步单独做、单独报结果：放在保存**之后**，是因为保存若被校验拦下就一个字节
    // 都不该动；而这一步失败也只是「本程序记住了、npm 那边没改」，如实说明即可。
    try {
      const cacheMsg = await invoke('apply_npm_cache', { dir: cfg.npm_cache_dir || null });
      appendLog('launcher', '[launcher] ' + cacheMsg);
      toast(cacheMsg);
      refreshNpmCacheInfo();
    } catch (e) {
      toast(t('toast_npm_cache_fail', e), true);
      appendLog('launcher', t('toast_npm_cache_fail', e));
    }
    // 用户环境变量 DSH_HOME 与 npm 缓存同款：保存之后单独做、单独报结果 —— 但它已经
    // **没有开关**：后端按「家目录是不是默认值」自动决定写还是删（is_default_home_dir）。
    // 影响面只有**用户另外打开的终端** —— 本程序启动 DSH（日常与安全模式）时都会显式
    // 注入 DSH_HOME 覆盖继承值，所以这里写不写都不改变本程序自己的行为。
    try {
      const envRep = await invoke('apply_dsh_home_env', {
        dir: cfg.dsh_home_dir,
        prev: prevHome || null,
      });
      // 每次都进日志（「没动」也是个结论）；只有真改了注册表才弹 toast ——
      // toast 是单元素的，每次都弹会把上面那条 npm 缓存提示盖掉
      appendLog('launcher', '[launcher] ' + envRep.message);
      if (envRep.changed) toast(envRep.message);
      refreshHomeEnvInfo();
    } catch (e) {
      toast(t('toast_home_env_fail', e), true);
      appendLog('launcher', t('toast_home_env_fail', e));
    }
    // 工具栏模式即时生效（保存后的 config 里已带归一化后的值）
    applyToolbarMode();
    refreshButtons();
    hideModal('settings-modal');
    const st = await invoke('get_status');
    onStatus(st);
    // DSH 界面语言联动提示：settings.yaml 已由后端写入，DSH 重启后生效
    if (langChanged && ['running', 'running-external'].includes(status)) {
      appendLog('launcher', t('log_locale_synced'));
    }
  } catch (err) {
    toast(t('toast_save_fail', err), true);
  }
}

function onPathPicked(p) {
  if (!p || !p.path) return;
  const target = document.querySelector(`[data-kind="${p.kind}"]`);
  if (target) {
    const input = $(target.dataset.target);
    if (input) {
      input.value = p.path;
      // 「浏览」选过目录 = 用户已经表达过意愿：别让后续检测把默认值覆盖回来
      markDirTouched(input);
      if (input.id === 'wiz-dsh-dir') refreshDshCmd();
      // 家目录改了就当场亮出搬家警告（「浏览」是用户真正动手搬家的入口，不能只等他手输）
      if (input.id === 'set-home-dir') refreshHomeMoveWarn();
    }
  }
}

// ---------- 版本（显示在工具栏右侧：本地 → 最新，并在有新版本时挂「可更新」标记） ----------

// 最近一次 check_versions 的结果（{ local, latest, next }）；
// 状态栏重绘（语言切换后）与「更新 DSH」弹窗里的频道版本号都取这里，不重新查 registry。
let lastVersionInfo = null;

// npm dist-tag 里的「最新可用版本」：next 频道比 latest 更新，所以只要 next 存在且
// 与 latest 不同，就以 next 为准；否则（没有 next 标签，或两个标签指向同一版本）用 latest。
function newestVersion(latest, next) {
  const l = latest ? latest.trim() : null;
  const n = next ? next.trim() : null;
  if (n && n !== l) return { version: n, tag: 'next' };
  if (l) return { version: l, tag: 'latest' };
  if (n) return { version: n, tag: 'next' };
  return null;
}

// 状态栏版本显示。三种形态：
//   本地 == 最新        → 「0.1.1（已是最新）」
//   本地 != 最新        → 「0.1.1 → 0.2.0」+ 高亮，并挂「可更新」/「可更新 next」标记
//   拿不到远端版本号     → 「0.1.1（更新状态未知）」；本地版本都没有时显示「未知」
// 比较只用字符串相等（与旧版一致）：注册表给出的就是这两个频道的准确值。
function renderVersion(local, latest, next) {
  const el = $('ver-val');
  const badge = $('ver-badge');
  badge.classList.add('hidden');
  badge.textContent = '';
  const newest = newestVersion(latest, next);
  const loc = local ? local.trim() : null;
  if (loc && newest) {
    if (loc === newest.version) {
      el.textContent = t('ver_latest_suffix', loc);
    } else {
      el.textContent = t('ver_arrow', loc, newest.version);
      badge.textContent = newest.tag === 'next' ? t('ver_badge_next') : t('ver_badge_update');
      badge.classList.remove('hidden');
      badge.classList.toggle('badge-next', newest.tag === 'next');
    }
  } else if (loc) {
    // 能读到本地 DSH 版本但查不到最新版：明确提示更新状态未知，
    // 避免用户把「只显示本地版本」误当成「已是最新」
    el.textContent = t('ver_local_only', loc);
  } else {
    // 本地 DSH 版本未知时不再单独显示 npm 上的最新版本——那会被误读为
    // 「npm 有新版本」；用户只关心 DSH 是否有新版本，此时无从比较，显示未知（原因在日志）
    el.textContent = t('ver_unknown2');
  }
}

// 自动检查入口：程序启动/重启、DSH 服务启动/重启完成、连接外部服务时触发；
// 检查进行中或处于 60 秒冷却期内时直接跳过，避免重复请求 npm registry
function autoCheckVersions() {
  if (checkingVersion) return;
  if (Date.now() - lastVersionCheckAt < 60_000) return;
  checkVersions();
}

async function checkVersions() {
  if (checkingVersion) return;
  checkingVersion = true;
  $('ver-val').textContent = t('ver_querying');
  appendLog('launcher', t('log_ver_querying'));
  try {
    const info = await invoke('check_versions');
    lastVersionInfo = info;
    if (info.local) appendLog('launcher', t('log_ver_local', info.local));
    if (info.latest) appendLog('launcher', t('log_ver_latest', info.latest));
    if (info.next) appendLog('launcher', t('log_ver_next', info.next));
    const newest = newestVersion(info.latest, info.next);
    if (newest && info.local && info.local.trim() !== newest.version) {
      appendLog(
        'launcher',
        t(newest.tag === 'next' ? 'log_ver_update_avail_next' : 'log_ver_update_avail',
          info.local.trim(), newest.version),
      );
    }
    if (info.error) appendLog('launcher', t('log_ver_error', info.error));
    renderVersion(info.local, info.latest, info.next);
  } catch (err) {
    renderVersion(null, null, null);
    appendLog('launcher', t('log_ver_fail', err));
  } finally {
    checkingVersion = false;
    lastVersionCheckAt = Date.now();
  }
}

// ---------- 更新 / 切换版本频道 ----------

// 当前弹窗里选中的频道（'latest' | 'next'）
function selectedTag() {
  const el = document.querySelector('input[name="upd-channel"]:checked');
  return el && el.value === 'next' ? 'next' : 'latest';
}

// npm 侧真正被安装的规格：<包名>@<频道>，由 registry 解析成具体版本号
function updateCmdText(tag) {
  return `"${config.npm_path}" install -g ${config.package_name}@${tag}`;
}

function renderUpdateCmd() {
  $('update-cmd-preview').textContent = updateCmdText(selectedTag());
}

// 单个频道行的版本号 + 说明标记。拿不到该频道的版本号时显示「—」，
// 但仍允许选择：装的是标签，具体版本由 npm 在安装时向 registry 解析。
// 标记按「版本号」判断而不是按「频道名」判断：两个频道指向同一版本时，
// 两边都不能说对方「较旧」（next === latest 是很常见的状态）。
function fillChannel(tag, version, newestVer, local) {
  const verEl = $('upd-ver-' + tag);
  const flag = $('upd-flag-' + tag);
  flag.textContent = '';
  const v = version ? version.trim() : null;
  verEl.textContent = v || t('upd_ver_unknown');
  let key = null;
  if (v) {
    if (local && v === local) key = 'upd_flag_current';
    else if (newestVer && v === newestVer) key = 'upd_flag_newest';
    else if (newestVer) key = 'upd_flag_older';
  }
  if (key) {
    flag.textContent = t(key);
    flag.className = 'upd-ch-flag ' + (key === 'upd_flag_current' ? 'cur' : key === 'upd_flag_newest' ? 'new' : 'old');
  } else {
    flag.className = 'upd-ch-flag hidden';
  }
}

// 填充弹窗内所有动态文案。resetSelection = true 时按「更新的频道」预选单选项
// （打开弹窗时）；false 只按当前选择重绘文字（语言切换时），不覆盖用户已经点选的频道。
function renderUpdateModal(resetSelection) {
  if (!config) return;
  const v = lastVersionInfo || {};
  const local = v.local ? v.local.trim() : null;
  const newest = newestVersion(v.latest, v.next);
  const newestTag = newest ? newest.tag : null;
  if (resetSelection) {
    // 两个频道始终可选（有人想试更新的东西，也有人想退回旧的稳定版），
    // 默认选中「更新的那个」
    const preferred = newestTag || 'latest';
    $('upd-tag-latest').checked = preferred === 'latest';
    $('upd-tag-next').checked = preferred === 'next';
  }
  fillChannel('latest', v.latest, newest ? newest.version : null, local);
  fillChannel('next', v.next, newest ? newest.version : null, local);
  // 备份提醒：无论哪个方向都建议备份，这里显示当前实际生效的家目录路径
  $('upd-backup-dir').textContent = config.dsh_home_dir || t('upd_backup_dir_unknown');
  renderUpdateCmd();
}

function confirmUpdate() {
  renderUpdateModal(true);
  showModal('update-modal');
}

async function doUpdate() {
  const tag = selectedTag();
  hideModal('update-modal');
  if (updating) return;
  updating = true;
  refreshButtons();
  // 立刻在页面上显示更新进度面板并清空上次输出：
  // 后端在真正进入 updating 状态前还要先停掉 DSH（会先经过 stopping/idle），
  // 靠状态事件显示会闪一下；update-log 事件的输出由 appendLog 实时镜像到这里。
  const panel = $('update-progress');
  $('update-progress-log').textContent = '';
  $('update-progress-text').textContent = t('upd_prog_prepare');
  panel.classList.remove('hidden');
  appendLog('update', t('log_update_begin', tag));
  try {
    await invoke('update_dsh', { tag });
  } catch (err) {
    updating = false;
    refreshButtons();
    panel.classList.add('hidden');
    toast(t('toast_update_fail_start', err), true);
    appendLog('update', t('log_update_start_fail', err));
  }
}

/// 更新进行中的实时进度（Rust 端每秒回报一次，文案已按界面语言本地化）
function onUpdateProgress(p) {
  const el = $('update-progress-text');
  if (el) el.textContent = p.message;
}

async function onUpdateFinished(p) {
  updating = false;
  refreshButtons();
  // 更新结束（无论成败）收起页面进度面板；完整输出仍保留在「日志」里
  $('update-progress').classList.add('hidden');
  if (p.success) {
    appendLog('update', t('log_update_done_restart'));
    toast(t('toast_update_restarting'));
    try {
      await invoke('start_dsh');
      // 更新后强制刷新一次版本显示（不走冷却，状态栏立即反映新版本）
      await checkVersions();
    } catch (err) {
      appendLog('launcher', t('log_restart_fail', err));
    }
  } else {
    toast(t('toast_update_failed_detail', p.message), true);
  }
}

// ---------- 卸载 DSH（首选项最底部） ----------
//
// 为什么不是「让用户自己去终端敲 npm uninstall -g」：npm 能删掉包与启动脚本，但**不会**
// 回收变空的 @deepseek-ai scope 目录，也不知道 ~/.npmrc 里那行 prefix= 是本程序写的。
// 这两件收尾只有本程序能做 —— 所以界面上给一个入口，由后端删完之后负责收尾。
//
// 流程：确认框（uninstall-modal）→ 执行 → 追问要不要顺带卸载 pnpm（uninstall-pnpm-modal）
// → 用户答完即最终结果页（同一个弹窗的第二态）。

let uninstalling = false;

/// 确认框里显示的「将执行」命令 —— 必须与后端真正执行的那条**逐字一致**
/// （`npm uninstall -g [--prefix "<目录>"] <包名>`，见 process.rs::run_npm_uninstall）。
/// 目标目录取 DSH 当前实际所在的目录；取不到时就不显示 --prefix（npm 会用自己解析的全局目录）。
function uninstallCommand() {
  const npm = '"' + ((config && config.npm_path) || 'npm') + '"';
  const pkg = (config && config.package_name) || '@deepseek-ai/dsh';
  const dir = dshInstallDir();
  return dir ? `${npm} uninstall -g --prefix "${dir}" ${pkg}` : `${npm} uninstall -g ${pkg}`;
}

/// 从 `dsh.cmd` 的完整路径取它所在的目录（= npm 的全局目录，也就是卸载目标）。
function dshInstallDir() {
  const p = ((config && config.dsh_path) || '').replace(/[\\/]+$/, '');
  const i = Math.max(p.lastIndexOf('\\'), p.lastIndexOf('/'));
  return i > 1 ? p.slice(0, i) : '';
}

/// 打开首选项时刷新「维护」那一块的可点状态（确认框里那条命令在打开时现场拼）
function refreshUninstallCmd() {
  const btn = $('btn-uninstall-dsh');
  if (!btn) return;
  btn.disabled = updating || !config || !config.dsh_exists;
}

/// 卸载 pnpm 用的命令，与后端 `uninstall_pnpm` 真正执行的那条**逐字一致**。
///
/// 必须带上 `--prefix`：pnpm 与 DSH 装在同一个 npm 全局目录里，少了这个参数就会去
/// npm 的默认全局目录里删 —— 那里没装 pnpm，npm 只回一句 `up to date` 并**返回退出码 0**，
/// 于是界面报「已成功卸载」而 pnpm 一个字节没动（真机现场，日志里那次正是 `up to date`）。
///
/// 目录来源与执行时**同一个表达式**（`pnpmUninstallDir()`）：弹窗里显示的命令与实际
/// 发出的那个目录不能有第二种算法，否则又会「说一套做一套」。
function pnpmUninstallDir() {
  return (uninstallDir || dshInstallDir()).replace(/[\\/]+$/, '');
}

function uninstallPnpmCommand() {
  const npm = '"' + ((config && config.npm_path) || 'npm') + '"';
  const d = pnpmUninstallDir();
  return d ? `${npm} uninstall -g --prefix "${d}" pnpm` : `${npm} uninstall -g pnpm`;
}

/// 把后端返回的结构化条目渲染成人话列表（路径一律走 textContent，绝不拼 innerHTML）
function uninstallItemText(it) {
  // npm 配置那一行有四种处置（removed / restored / unchanged / kept_manual），
  // 后端把它们放在 status 里，前端各自给一句能指导下一步动作的话 —— 混成一句「已保留」
  // 用户既不知道发生了什么，也不知道该怎么办（真机现场就是这么反馈的）。
  const statusKey =
    it.kind === 'npmrc'
      ? 'uninstall_status_npmrc_' + (it.status || 'unchanged')
      : 'uninstall_status_' + (it.status || 'removed');
  const label = t('uninstall_item_' + (it.kind || 'package'), it.path);
  return it.path ? `${label} — ${t(statusKey)}` : label;
}

/// 渲染结果列表。`rep` = 后端返回的 UninstallReport，`removedPkg` = 是否把「包本身已删除」
/// 也列进去（失败时不该列 —— 那时包还在）。
function renderUninstallItems(list, rep, removedPkg) {
  list.innerHTML = '';
  if (removedPkg && rep) {
    const li0 = document.createElement('li');
    li0.textContent =
      t('uninstall_item_package', rep.package_name) + ' — ' + t('uninstall_status_removed');
    list.appendChild(li0);
  }
  for (const it of (rep && rep.items) || []) {
    const li = document.createElement('li');
    li.textContent = uninstallItemText(it);
    list.appendChild(li);
  }
}

function openUninstallConfirm() {
  if (!config) return;
  // 诊断留痕：这条路径上「点了没反应」最难查（界面无变化、Rust 侧也没收到命令），
  // 日志里有这一行就能立刻分清是「弹窗没开」还是「开了但被内嵌页盖住」。
  appendLog('launcher', '[uninstall] 打开卸载确认框（目标目录：' + (dshInstallDir() || 'npm 默认目录') + '）');
  // 从首选项进入：先关掉首选项，视觉上「一件事一个弹窗」
  hideModal('settings-modal');
  hideModal('uninstall-pnpm-modal');
  $('uninstall-cmd').textContent = uninstallCommand();
  $('uninstall-progress').classList.add('hidden');
  $('uninstall-result-list').innerHTML = '';
  $('uninstall-progress-text').textContent = t('uninstall_running');
  $('uninstall-fail-hint').classList.add('hidden');
  $('btn-uninstall-confirm').disabled = false;
  $('btn-uninstall-cancel').disabled = false;
  showModal('uninstall-modal');
}

async function doUninstallDsh() {
  if (uninstalling) return;
  uninstalling = true;
  // 诊断留痕（见 openUninstallConfirm 的说明）：从这里开始才有后端动作，
  // 日志里有没有这一行，就能区分「用户没点到确认」与「后端没干活」。
  appendLog('launcher', '[uninstall] 用户确认卸载，开始执行');
  $('btn-uninstall-confirm').disabled = true;
  $('btn-uninstall-cancel').disabled = true;
  $('uninstall-fail-hint').classList.add('hidden');
  $('uninstall-progress').classList.remove('hidden');
  $('uninstall-progress-text').textContent = t('uninstall_running');
  try {
    const rep = await invoke('uninstall_dsh');
    renderUninstallItems($('uninstall-result-list'), rep, rep.success);
    if (!rep.success) {
      $('uninstall-fail-hint').classList.remove('hidden');
      $('btn-uninstall-confirm').disabled = false;
      $('btn-uninstall-cancel').disabled = false;
      uninstalling = false;
      toast(t('toast_uninstall_fail', rep.output || ''), true);
      return;
    }
    // 成功后：记下卸载目标（追问弹窗要用它拼 pnpm 命令），并刷新配置与状态显示。
    // 只刷配置：`get_config` 会重新检测一次（后端已 invalidate），config.dsh_exists /
    // dsh_path 随之变成「未安装」，首选项那个按钮也跟着禁用 —— 这正是我们要的界面事实。
    uninstallDir = rep.dir || uninstallDir;
    await refreshConfig();
    try {
      onStatus(await invoke('get_status'));
    } catch (_) { /* 状态事件也会推一次，拿不到不影响卸载结论 */ }
    toast(t('toast_uninstall_ok'));
    uninstalling = false;
    hideModal('uninstall-modal');
    openPnpmPrompt();
  } catch (err) {
    uninstalling = false;
    $('uninstall-fail-hint').classList.remove('hidden');
    $('btn-uninstall-confirm').disabled = false;
    $('btn-uninstall-cancel').disabled = false;
    toast(t('toast_uninstall_start_fail', err), true);
  }
}

/// 追问弹窗（问 + 最终结果页这两态共用同一个弹窗）
function openPnpmPrompt() {
  $('uninstall-final-title').textContent = t('uninstall_final_title');
  $('uninstall-pnpm-ask').classList.remove('hidden');
  $('uninstall-final-body').classList.add('hidden');
  $('uninstall-pnpm-actions').classList.remove('hidden');
  $('uninstall-final-actions').classList.add('hidden');
  $('uninstall-pnpm-fail').classList.add('hidden');
  $('btn-uninstall-pnpm-yes').disabled = false;
  $('btn-uninstall-pnpm-no').disabled = false;
  $('uninstall-pnpm-cmd').textContent = uninstallPnpmCommand();
  showModal('uninstall-pnpm-modal');
}

/// 最终结果页：pnpmRemoved = 用户选择了「一并卸载」并且它成功了
function showUninstallFinal(pnpmRemoved) {
  $('uninstall-pnpm-ask').classList.add('hidden');
  $('uninstall-pnpm-actions').classList.add('hidden');
  $('uninstall-final-body').classList.remove('hidden');
  $('uninstall-final-actions').classList.remove('hidden');
  const msg = $('uninstall-final-msg');
  msg.textContent = '';
  if (pnpmRemoved) {
    // 「DSH 和 pnpm 已成功卸载。」
    msg.textContent = t('uninstall_done_both');
    $('uninstall-final-manual').classList.add('hidden');
  } else {
    // 「DSH 已成功卸载，日后如需卸载管理 DSH 插件用的 pnpm，可以在终端执行：」
    msg.textContent = t('uninstall_done_dsh_only');
    $('uninstall-final-manual').classList.remove('hidden');
    $('uninstall-final-cmd').textContent = uninstallPnpmCommand();
  }
}

async function doUninstallPnpm() {
  if (uninstalling) return;
  uninstalling = true;
  $('btn-uninstall-pnpm-yes').disabled = true;
  $('btn-uninstall-pnpm-no').disabled = true;
  $('uninstall-pnpm-fail').classList.add('hidden');
  try {
    // 目录必须显式带给后端：卸载 DSH 成功后后端已把 dsh_path 清空，它自己再也推导不出
    // 那个全局目录 —— 第一版就是这样退化成 `npm uninstall -g pnpm`（无 --prefix）的。
    // 传的正是弹窗里显示给用户的那个目录（同一个表达式 pnpmUninstallDir）。
    const rep = await invoke('uninstall_pnpm', { dir: pnpmUninstallDir() });
    uninstalling = false;
    if (rep && rep.success) {
      showUninstallFinal(true);
    } else {
      // 失败不假装成功：留在追问态，让用户可以「保留 pnpm」继续收尾。
      // rep.items 里可能带着「pnpm 其实装在哪」这条信息（它不在 DSH 的全局目录里时）。
      $('uninstall-pnpm-fail').classList.remove('hidden');
      $('btn-uninstall-pnpm-yes').disabled = false;
      $('btn-uninstall-pnpm-no').disabled = false;
      toast(t('toast_uninstall_fail', pnpmFailDetail(rep)), true);
    }
  } catch (err) {
    uninstalling = false;
    $('uninstall-pnpm-fail').classList.remove('hidden');
    $('btn-uninstall-pnpm-yes').disabled = false;
    $('btn-uninstall-pnpm-no').disabled = false;
    toast(t('toast_uninstall_fail', err), true);
  }
}

/// 卸载 pnpm 失败时给用户的具体原因。
/// 后端能分辨「它其实装在别处」（`still_elsewhere`，path 是实际路径）与「试过但没删成」
/// （`failed`，output 是 npm 的话）—— 这两种的下一步动作完全不同，提示也要分开。
function pnpmFailDetail(rep) {
  const it = rep && (rep.items || []).find((x) => x.status === 'still_elsewhere');
  if (it && it.path) return t('uninstall_pnpm_still_elsewhere', it.path);
  return (rep && rep.output) || '';
}

/// 卸载目标的记忆（DSH 卸载后 dsh_path 已被清空，追问 pnpm 时还要用它拼命令）
let uninstallDir = '';

// ---------- 首次运行引导向导（环境检查 + 引导安装，可随时跳过） ----------

const wiz = {
  active: false,
  detection: null,
  busy: false, // 是否有引导安装任务在跑
};

async function runSetupWizard() {
  wiz.active = true;
  $('setup-wizard').classList.remove('hidden');
  $('wiz-log').classList.add('hidden');
  $('wiz-log').textContent = '';
  $('wiz-progress').classList.add('hidden');
  // 第一步：语言选择（固定双语展示）。选完语言再检测环境，
  // 这样检测/安装/进度文案从一开始就是用户所选语言。
  $('wiz-step-lang').classList.remove('hidden');
  $('wiz-btn-finish').classList.add('hidden');
}

async function onWizLanguage(lang) {
  // 先切前端词典并刷新 DOM，后续向导文案立即变为所选语言
  I18N.setLang(lang);
  I18N.applyDom();
  $('wiz-step-lang').classList.add('hidden');
  // 后端：托盘文案、DSH settings.yaml、sidecar 持久化（best-effort，失败不阻断）
  try {
    await invoke('set_language', { lang });
  } catch (e) {
    toast(t('toast_lang_fail', e), true);
  }
  appendLog('launcher', t('wiz_first_run_log'));
  await wizDetect();
}

async function wizDetect() {
  setWizProgress(true, t('wiz_detect_env_progress'));
  try {
    wiz.detection = await invoke('detect_environment');
    lastEnvDetection = wiz.detection;
    prefillNodeDir(wiz.detection);
    prefillDshDir(wiz.detection);
  } catch (e) {
    toast(t('wiz_env_fail', e), true);
    wiz.detection = null;
  }
  setWizProgress(false);
  renderWiz();
}

// ---------- 「安装位置」的预填 ----------
// 默认值由后端按本机 %ProgramFiles% 算出（Windows 装在 D 盘时默认目录也在 D 盘），
// 前端不写死路径。预填而不是留空：绝大多数用户要的就是默认位置。
// 唯一的规则是**用户一旦动过就再也不覆盖** —— 改成 D:\nodejs 之后再点一次「重新检测」，
// 输入框不能被默认值悄悄改回去（那等于把用户的输入吃掉）。
let nodeDirTouched = false;

// DSH 那一格同理，只是默认值来自 npm 自己（`npm config get prefix`，问不到才回落
// %APPDATA%\npm，见 detect.rs::default_npm_prefix）—— 用户在 .npmrc 里配过 prefix 时，
// 预填的也是他真正会装到的位置。
let dshDirTouched = false;

function prefillNodeDir(det) {
  const el = $('wiz-node-dir');
  if (!el || !det || nodeDirTouched) return;
  const want = (det.node_default_dir || '').trim();
  if (want) el.value = want;
}

function prefillDshDir(det) {
  const el = $('wiz-dsh-dir');
  if (!el || !det || dshDirTouched) return;
  const want = (det.npm_default_prefix || '').trim();
  if (want) el.value = want;
}

/// 用户手动改过 / 用「浏览」选过目录 → 从此不再被默认值覆盖（两个输入框各自记一笔）。
function markDirTouched(input) {
  if (!input) return;
  if (input.id === 'wiz-node-dir') nodeDirTouched = true;
  if (input.id === 'wiz-dsh-dir') dshDirTouched = true;
}

/// 向导里展示（并可复制）的那条安装命令 —— 必须与 Rust 端真正执行的命令**逐字一致**：
/// 参数顺序见 process.rs::dsh_install_args（`install -g [--prefix "<目录>"] <包名>`）。
/// 位置留空时**不带** `--prefix`，那正是「用 npm 默认全局目录」的表达方式。
function dshInstallCommand(npmPath) {
  const npm = '"' + (npmPath || 'npm') + '"';
  const pkg = (config && config.package_name) || '@deepseek-ai/dsh';
  const el = $('wiz-dsh-dir');
  const dir = el && el.value ? el.value.trim() : '';
  return dir
    ? `${npm} install -g --prefix "${dir}" ${pkg}`
    : `${npm} install -g ${pkg}`;
}

/// 位置改了要立刻反映到上面那条「将执行以下命令」上：用户是照着它核对/复制的，
/// 让它停在旧内容上等于展示一条与真正执行不符的命令。
function refreshDshCmd() {
  const el = $('wiz-dsh-cmd');
  if (!el) return;
  const det = wiz.detection || lastEnvDetection;
  el.textContent = dshInstallCommand(det && det.npm_path);
}

/// 向导「缺少 pnpm」一步展示的命令 —— 与 Rust 端 install_pnpm_blocking 真正执行的
/// `npm install -g pnpm` 逐字一致（pnpm 不带 --prefix：它必须落在 npm 自己的全局目录里，
/// 那通常已经在 PATH 中，装到别处反而要额外处理 PATH）。
function pnpmInstallCommand(npmPath) {
  const npm = '"' + (npmPath || 'npm') + '"';
  return `${npm} install -g pnpm`;
}

function setWizFlag(flagId, pathId, found, detail) {
  const f = $(flagId);
  f.textContent = found ? t('wiz_installed') : t('wiz_notfound');
  f.className = 'flag ' + (found ? 'ok' : 'bad');
  $(pathId).textContent = detail || '';
}

// ---------- Node.js 版本下限判定（与 detect.rs 的 NODE_MIN_VERSION 同源） ----------
// 后端在检测结果里给出状态（supported / old / unknown）与最低版本号，
// 前端只负责取用与展示，阈值不在这里写死——以后程序提高下限时只改 Rust 常量。

/// 从检测结果取出「版本状态」字段，兼容三种历史/异常形态，取不到返回 ''。
function nodeMinState(det) {
  if (!det) return '';
  const s = det.node_min_state;
  return (typeof s === 'string') ? s : '';
}

/// 本机 Node 是否确实低于下限（只有后端明确说 "old" 才算，未知一律不告警）。
function nodeIsTooOld(det) {
  return nodeMinState(det) === 'old';
}

/// 检测到但版本号读不出/解析不了（后端判 "unknown" 且确实找到了 node）。
function nodeVersionUnknown(det) {
  return !!det && !!det.node_found && nodeMinState(det) === 'unknown';
}

/// 告警文案里的两个版本号：当前版本、最低版本（最低版本来自后端，缺省回落常量）。
function nodeVersionPair(det) {
  const current = (det && det.node_version ? String(det.node_version) : '').trim() || '?';
  const min = (det && det.node_min_version ? String(det.node_min_version) : '').trim() || 'v22.19.0';
  return [current, min];
}

// ---------- pnpm 版本下限判定（与 detect.rs 的 PNPM_MIN_VERSION 同源） ----------
// 本程序对 pnpm 有**硬要求**：必须 ≥ 下限，缺失或低于下限都要先装上 / 更新掉
// （用户 2026 本轮拍板：pnpm 必须 ≥ 10，低了就必须更新 —— 这也推翻了早先
// 「pnpm 缺失不阻断完成」的决定）。阈值仍只写在 Rust 常量里，前端只负责取用与展示。

/// 从检测结果取出 pnpm 的「版本状态」字段（supported / old / unknown），取不到返回 ''。
function pnpmMinState(det) {
  if (!det) return '';
  const s = det.pnpm_min_state;
  return (typeof s === 'string') ? s : '';
}

/// pnpm 是否确实低于下限：只有后端明确说 "old" 才算。
/// **读不出版本不算** —— 拦截「完成」要的是「证明它不达标」，读不出证明不了，
/// 拿 unknown 当过低会把一台 pnpm 其实够用的机器堵死在向导里。
function pnpmIsTooOld(det) {
  return pnpmMinState(det) === 'old';
}

/// pnpm 是否达标：**在场，且没被明确判定为过旧**。
/// 读不出版本（unknown）按 Node 同款处理 —— 状态行给个「? 版本未知」的警示，
/// 但**不拦「完成」**：拦下要的是「证明它不达标」，读不出证明不了；而且这台机器
/// 上点「更新」也未必能让版本号变得可读，拦了就是一个修不了的死胡同。
function pnpmOk(det) {
  if (!det || !det.pnpm_found) return false;
  return pnpmMinState(det) !== 'old';
}

/// pnpm 是否正拦着「进入主界面」：node+npm 都在场（= 这一步可操作）而 pnpm 未达标。
/// 与 renderWiz 的禁用逻辑、wizFinish 的收口逻辑共用一个判定，免得三处走样。
function pnpmBlocksFinish(det) {
  return !!det && !!det.node_found && !!det.npm_found && !pnpmOk(det);
}

/// 告警文案里的两个版本号：当前版本、最低版本（最低版本来自后端，缺省回落常量）。
function pnpmVersionPair(det) {
  const current = (det && det.pnpm_version ? String(det.pnpm_version) : '').trim() || '?';
  const min = (det && det.pnpm_min_version ? String(det.pnpm_min_version).trim() : '') || 'v10';
  return [current, min];
}

/// 版本号的括号包裹：中文用全角括号，英文用半角（两处都靠它，避免中英混排）。
function verTag(v) {
  if (!v) return '';
  return I18N.lang === 'en' ? ' (' + v + ')' : '（' + v + '）';
}

/// 只用于「我们自己拼 HTML」的场景（模板里的 <b> 是固定标签，插入的版本号必须转义）。
function escapeHtml(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/// 用户是否已经选过「保留该版本，仍要继续」（后端比对的也是这个键）。
/// ConfigReport 直接展开 Config 字段，所以这里读得到；老后端没有该字段时按未确认处理。
function nodeAckValue() {
  return config && typeof config.node_min_ack === 'string' ? config.node_min_ack : '';
}

/// 当前检测结果是否处于「已确认继续」状态：用户确认过、且确认的就是现在这条下限。
/// 判定放在前端是为了让「仍要继续」当场生效（后端同款判定的权威副本在 process.rs）。
function nodeAckConfirmed(det) {
  const ack = nodeAckValue();
  if (!ack) return false;
  const min = (det && det.node_min_version) ? String(det.node_min_version).trim() : 'v22.19.0';
  return ack.trim() === min.replace(/^v/i, '');
}

function renderWiz() {
  const d = wiz.detection;
  if (!d) return;

  // Node.js 一行：路径 + 版本号；版本过低时状态列直接说清楚，不再显示乐观的「✔ 已安装」
  if (d.node_found) {
    const v = (d.node_version || '').trim();
    if (nodeIsTooOld(d)) {
      setWizFlag('wiz-node-flag', 'wiz-node-path', false, '');
      $('wiz-node-flag').textContent = t('wiz_node_flag_old');
      $('wiz-node-flag').className = 'flag warn';
      // 路径列改放「当前 → 最低要求」，一眼看出差在哪
      $('wiz-node-path').textContent = (v || '?') + ' → ' + (d.node_min_version || 'v22.19.0') + '+';
    } else if (nodeVersionUnknown(d)) {
      setWizFlag('wiz-node-flag', 'wiz-node-path', false, '');
      $('wiz-node-flag').textContent = t('wiz_node_flag_unknown');
      $('wiz-node-flag').className = 'flag warn';
      $('wiz-node-path').textContent = d.node_path + verTag(v);
    } else {
      setWizFlag('wiz-node-flag', 'wiz-node-path', true, d.node_path + verTag(v));
    }
  } else {
    setWizFlag('wiz-node-flag', 'wiz-node-path', false, '');
  }
  setWizFlag('wiz-npm-flag', 'wiz-npm-path', d.npm_found, d.npm_path);
  setWizFlag('wiz-dsh-flag', 'wiz-dsh-path', d.dsh_found, d.dsh_path);
  // pnpm 一行：与 Node 同款三态 —— 过旧时状态列直接说清楚（「已安装」会掩盖
  // 「装了但版本不够」这个必须处理的事实），路径列改放「当前 → 最低要求」
  if (d.pnpm_found) {
    const pv = (d.pnpm_version || '').trim();
    if (pnpmIsTooOld(d)) {
      setWizFlag('wiz-pnpm-flag', 'wiz-pnpm-path', false, '');
      $('wiz-pnpm-flag').textContent = t('wiz_pnpm_flag_old');
      $('wiz-pnpm-flag').className = 'flag warn';
      const [pcur, pmin] = pnpmVersionPair(d);
      $('wiz-pnpm-path').textContent = pcur + ' → ' + pmin + '+';
    } else if (pnpmMinState(d) === 'unknown') {
      // 版本读不出：警示但不算过低（见 pnpmIsTooOld 的注释）
      setWizFlag('wiz-pnpm-flag', 'wiz-pnpm-path', false, '');
      $('wiz-pnpm-flag').textContent = t('wiz_pnpm_flag_unknown');
      $('wiz-pnpm-flag').className = 'flag warn';
      $('wiz-pnpm-path').textContent = d.pnpm_path + verTag(pv);
    } else {
      setWizFlag('wiz-pnpm-flag', 'wiz-pnpm-path', true, d.pnpm_path + verTag(pv));
    }
  } else {
    setWizFlag('wiz-pnpm-flag', 'wiz-pnpm-path', false, '');
  }

  // 「全部就绪」现在**包含 pnpm 达标**：缺 pnpm 或 pnpm 低于下限都不算就绪
  // （用户 2026 本轮拍板：pnpm 必须 ≥ 10，低了就必须更新）
  const allOK = d.node_found && d.npm_found && d.dsh_found && pnpmOk(d);

  // Node 步骤：缺 node/npm 时显示
  $('wiz-step-node').classList.toggle('hidden', wiz.busy || d.node_found);
  // DSH 步骤：node+npm 就绪但缺 DSH 时显示
  $('wiz-step-dsh').classList.toggle('hidden', wiz.busy || !d.node_found || !d.npm_found || d.dsh_found);
  // pnpm 步骤：node+npm 就绪、而 pnpm **缺失或低于下限**时显示（过旧也要走这一步更新）。
  // 要求 npm 在场：pnpm 只能由 `npm install -g pnpm` 装，npm 缺失时给出按钮必然失败。
  const pnpmStepShown = !wiz.busy && d.node_found && d.npm_found && !pnpmOk(d);
  $('wiz-step-pnpm').classList.toggle('hidden', !pnpmStepShown);
  if (pnpmStepShown) {
    // 缺失 vs 过旧：标题、正文、按钮各自换一套文案（过旧要给当前/最低版本号）
    const pnpmOld = pnpmIsTooOld(d);
    const [pcur, pmin] = pnpmVersionPair(d);
    $('wiz-pnpm-step-title').textContent = t(pnpmOld ? 'wiz_step_pnpm_title_old' : 'wiz_step_pnpm_title');
    $('wiz-pnpm-body-text').innerHTML = pnpmOld
      ? t('wiz_step_pnpm_body_old_html', escapeHtml(pcur), escapeHtml(pmin))
      : t('wiz_step_pnpm_body_text');
    $('wiz-pnpm-body-note').textContent = t(pnpmOld ? 'wiz_step_pnpm_body_old_note' : 'wiz_step_pnpm_body_note');
    $('wiz-btn-install-pnpm').textContent = t(pnpmOld ? 'wiz_btn_update_pnpm' : 'wiz_btn_install_pnpm');
    $('wiz-pnpm-cmd').textContent = pnpmInstallCommand(d.npm_path);
  }

  // Node 版本过低告警：node 在、但版本低于下限（用户已确认继续时不弹；版本读不出时
  // 只显示警示、不显示一键升级以外的引导——它可能其实是够的）
  const showOld = !wiz.busy && d.node_found && nodeIsTooOld(d) && !nodeAckConfirmed(d);
  $('wiz-step-node-old').classList.toggle('hidden', !showOld);
  if (showOld) {
    const [cur, min] = nodeVersionPair(d);
    const detail = $('wiz-node-old-detail');
    // 用真实版本号重绘（data-i18n-html 的初始文案只是兜底）
    detail.innerHTML = t('wiz_node_old_detail_html', escapeHtml(cur), escapeHtml(min));
    $('wiz-node-unknown-note').classList.toggle('hidden', !nodeVersionUnknown(d));
    // 首次弹出时把日志区展开：升级/校验的输出就写在里面
    $('wiz-log').classList.remove('hidden');
  }

  // 引导信息（wiz-node-url / wiz-dsh-cmd 可能在 applyDom 后被重建，这里重新赋值即可）
  const urlEl = $('wiz-node-url');
  if (urlEl) urlEl.textContent = d.node_msi_url || 'https://nodejs.org/en/download';
  const cmdEl = $('wiz-dsh-cmd');
  if (cmdEl) cmdEl.textContent = dshInstallCommand(d.npm_path);
  const pnpmCmdEl = $('wiz-pnpm-cmd');
  if (pnpmCmdEl) pnpmCmdEl.textContent = pnpmInstallCommand(d.npm_path);

  // 完成按钮：全部就绪 → 直接进入；有缺失 → 等同「跳过」；
  // 但 **pnpm 缺失或低于下限时直接拦住**（用户 2026 本轮拍板：pnpm 必须 ≥ 10，
  // 低了就必须更新 —— 推翻早先「pnpm 缺失不阻断完成」的决定）。
  // 只在「这一步确实可操作」时拦：node/npm 不全时 pnpm 根本装不上，硬拦会把向导
  // 走死（那种机器本来就得先走「安装 Node」那一步）。
  const pnpmBlocking = pnpmBlocksFinish(d);
  const finishBtn = $('wiz-btn-finish');
  finishBtn.disabled = pnpmBlocking;
  finishBtn.textContent = allOK ? t('wiz_all_ready')
    : (pnpmBlocking ? t('wiz_pnpm_required') : t('wiz_skip_go'));
  finishBtn.classList.remove('hidden');
  // 旁边那句默认提示也跟着换：说清「为什么现在点不了」，否则一枚禁用按钮看着像 bug
  const skipNote = $('wiz-skip-note');
  if (skipNote) {
    skipNote.textContent = pnpmBlocking
      ? t('wiz_pnpm_block_note', pnpmVersionPair(d)[1])
      : t('wiz_skip_note');
  }

  // 安装进行中禁用相关按钮
  ['wiz-btn-install-node', 'wiz-btn-recheck', 'wiz-btn-skip-node', 'wiz-btn-upgrade-node',
   'wiz-btn-node-manual', 'wiz-btn-node-ignore', 'wiz-btn-node-recheck',
   'wiz-btn-install-dsh', 'wiz-btn-copy-dsh-cmd', 'wiz-btn-recheck2', 'wiz-btn-skip-dsh',
   'wiz-btn-install-pnpm', 'wiz-btn-recheck-pnpm']
    .forEach((id) => { $(id).disabled = wiz.busy; });
}

function setWizProgress(show, text) {
  const el = $('wiz-progress');
  el.classList.toggle('hidden', !show);
  if (text) $('wiz-progress-text').textContent = text;
}

function onSetupStatus(p) {
  // 首选项的 Python 安装任务在跑时，进度归它（两者共用后端的 setup_busy，
  // 同一时刻只会有一个，所以这里可以直接让 Python 优先）
  if (pythonTask.active) {
    if (p.phase === 'download' || p.phase === 'install' || p.phase === 'verify') {
      setPythonProgress(true, p.message);
    }
    return;
  }
  if (!wiz.active) return;
  if (p.phase === 'download' || p.phase === 'install' || p.phase === 'verify') {
    setWizProgress(true, p.message); // 文案由 Rust 端按当前语言生成
    $('wiz-log').classList.remove('hidden');
  }
}

function onSetupResult(p) {
  // 首选项里发起的 Python 安装（基本安装 / 数据分析扩展包）：进度区在设置页里，
  // 与向导无关 —— 不碰 wiz.busy，也不触发向导的「重新检测」
  if (p.target && p.target.indexOf('python') === 0) {
    onPythonResult(p);
    return;
  }
  // 引导安装的入口只剩首次运行向导的「Node 版本过低」告警面板（首选项里的那处已移除），
  // 所以这里必然处于向导里；仍然不 return，避免事件早到时留下转不完的「处理中…」面板。
  wiz.busy = false;
  renderWiz();
  if (p.success) {
    setWizProgress(true, p.message);
    $('wiz-log').classList.remove('hidden');
    toast(p.message || t('wiz_installed'));
    // 成功后自动重新检测：版本达标即进入下一步
    setTimeout(() => wizDetect(), 600);
  } else {
    setWizProgress(false);
    $('wiz-log').classList.remove('hidden');
    toast(p.message || t('wiz_node_start_fail'), true);
    // pnpm 这条失败里有一类是「装上了，但版本仍不达标」（镜像/缓存给回旧版）：
    // 不重新检测的话，状态行会一直停在安装前的快照（写着「未找到 pnpm」，toast 却
    // 说装上了）——安全上无所谓（旧快照照样拦着「完成」），但用户得自己点
    // 「重新检测」才能看清真相，这里替他查一次。
    if (p.target === 'pnpm') setTimeout(() => wizDetect(), 600);
  }
}

/// 记忆/配置变化后重绘向导里的 Node 状态（版本行 + 告警面板）。
function refreshNodeIndicators() {
  if (wiz.active) renderWiz();
}

/// 「一键下载并安装最新 LTS」：只从首次运行向导的「Node 版本过低」告警面板发起
/// （首选项里的入口已移除，所以这里必然在向导中）。
/// 进度与结果由 setup-status / setup-result 事件驱动（Rust 端按当前语言输出）。
async function runNodeLtsInstall() {
  if (wiz.busy) {
    appendLog('launcher', t('log_node_install_busy'));
    toast(t('log_node_install_busy'), true);
    return;
  }
  wiz.busy = true;
  appendLog('launcher', t('log_node_install_manual'));
  renderWiz();
  setWizProgress(true, t('wiz_download_node'));
  $('wiz-log').classList.remove('hidden');
  try {
    // 显式传 null：升级这条路一律沿用现有安装目录，不换盘（同一个 ProductCode
    // 改 INSTALLDIR 会留下旧目录与旧的 PATH 条目；只有「首次安装」才让用户选目录）。
    // 键名是单词 dir —— 见 Rust 侧 setup_install_node 的注释（多词名会踩风格转换）。
    await invoke('setup_install_node', { dir: null });
  } catch (e) {
    wiz.busy = false;
    setWizProgress(false);
    renderWiz();
    toast(String(e), true);
  }
}

function wizFinish() {
  // 「完成」的**唯一收口**：完成按钮只是一枚 UI，Node / DSH 两步的「跳过，仍要进入」
  // 也调这里 —— 不在这儿挡一次，上面那枚禁用按钮形同虚设
  // （用户 2026 本轮拍板：pnpm 必须 ≥ 10，低了就必须更新）。
  const d = wiz.detection;
  if (pnpmBlocksFinish(d)) {
    toast(t('wiz_pnpm_block_note', pnpmVersionPair(d)[1]), true);
    // 把 pnpm 那一步顶到眼前：只弹一条 toast 的话，用户不知道该去点哪里
    renderWiz();
    return;
  }
  invoke('finish_setup')
    .then(async (report) => {
      config = report;
      $('port-val').textContent = config.port;
      applyAppearance(config.appearance);
      I18N.setLang(config.language);
      I18N.applyDom();
    })
    .catch((e) => toast(t('toast_setup_save_fail', e), true));
  $('setup-wizard').classList.add('hidden');
  wiz.active = false;
  appendLog('launcher', t('wiz_done_log'));
  postInit().catch((e) => appendLog('launcher', t('init_fail', e)));
}

// ---------- 绑定 ----------

function bindUI() {
  $('btn-start').onclick = () => invoke('start_dsh').catch((e) => toast(String(e), true));
  $('btn-stop').onclick = () => invoke('stop_dsh').catch((e) => toast(String(e), true));
  $('btn-restart').onclick = () => invoke('restart_dsh').catch((e) => toast(String(e), true));
  $('btn-update').onclick = confirmUpdate;
  // 安全模式：未进入 = 进入（后端会先完整停掉日常实例）；已进入 = 退出并重启日常
  $('btn-safe').onclick = () => { if (safeMode) exitSafeMode(); else enterSafeMode(); };
  // 徽标点击重开引导横幅（内容为最近一次进入的报告）
  $('safe-badge').onclick = () => {
    if (!safeMode) return;
    fillSafeModal(safeReport);
    showModal('safe-modal');
  };
  $('btn-safe-ok').onclick = () => hideModal('safe-modal');
  $('btn-safe-exit-modal').onclick = () => exitSafeMode();
  $('btn-safe-verify-close').onclick = () => hideModal('safe-verify-modal');
  // 修复验证失败 →「返回安全模式」：重新走完整进入流程（再次归档、重新借凭据）
  $('btn-safe-verify-back').onclick = () => { hideModal('safe-verify-modal'); enterSafeMode(); };
  $('btn-log').onclick = () => showModal('log-modal');
  $('btn-settings').onclick = openSettings;
  $('btn-cancel-settings').onclick = () => hideModal('settings-modal');
  $('btn-save-settings').onclick = saveSettings;
  $('btn-close-log').onclick = () => hideModal('log-modal');
  $('btn-clear-log').onclick = () => { logBody.innerHTML = ''; };
  $('btn-cancel-update').onclick = () => hideModal('update-modal');
  $('btn-confirm-update').onclick = doUpdate;
  // 切换频道时同步刷新「将执行」的命令预览
  document.querySelectorAll('input[name="upd-channel"]').forEach((r) => {
    r.onchange = renderUpdateCmd;
  });
  $('btn-connect').onclick = () => invoke('connect_existing').catch((e) => toast(String(e), true));
  $('btn-change-port').onclick = openSettings;

  // ---- 工具栏自动隐藏：进入触发条 / 工具栏即展开，离开工具栏 500ms 后收起 ----
  // 触发条只在自动隐藏模式下可见（固定显示时它是 display:none）。
  $('tb-hotzone').addEventListener('mouseenter', () => showToolbar());
  const toolbarEl = $('toolbar');
  toolbarEl.addEventListener('mouseenter', () => {
    toolbarHovered = true;
    showToolbar();          // 已展开时是空操作；内部会取消收起倒计时
    cancelToolbarHideTimer();
  });
  toolbarEl.addEventListener('mouseleave', () => {
    toolbarHovered = false;
    scheduleToolbarHide();
  });

  // 端口占用面板：重新检测端口（不杀任何进程，只探测）
  $('btn-recheck-port').onclick = async () => {
    try {
      const r = await invoke('check_port');
      if (!r.in_use) {
        toast(t('toast_port_free', r.port));
      } else {
        toast(t('toast_port_busy', r.port));
      }
    } catch (e) {
      toast(t('toast_recheck_fail', e), true);
    }
  };

  // 复制错误信息
  $('btn-copy-error').onclick = async () => {
    const ok = await copyText(lastErrorText || $('stage-line').textContent || '');
    toast(ok ? t('toast_copied_err') : t('toast_copy_fail'), !ok);
  };

  // 日志面板：打开日志目录 / 复制日志文本
  $('btn-open-logdir').onclick = () => invoke('open_log_dir').catch((e) => toast(String(e), true));
  $('btn-copy-log').onclick = async () => {
    const text = Array.from(logBody.children)
      .map((el) => el.textContent)
      .join('\n');
    const ok = await copyText(text);
    toast(ok ? t('toast_copied_log') : t('toast_copy_fail'), !ok);
  };

  // 设置页：自动检测 Node/npm/DSH 路径并回填输入框
  $('btn-autodetect').onclick = async () => {
    appendLog('launcher', t('log_detect_start'));
    try {
      const d = await invoke('detect_environment');
      lastEnvDetection = d; // 向导的「将执行以下命令」预览用它兜底
      // npm / dsh 程序路径没有输入框了：检测结果只用于日志与向导预览，不再回填界面
      appendLog('launcher', t('log_detect_done',
        d.node_path || t('wiz_notfound'),
        d.node_version ? '(' + d.node_version + ')' : '',
        d.npm_path || t('wiz_notfound'),
        d.dsh_path || t('wiz_notfound')));
      // 版本过低时把话说清楚：不只是「检测完成」，而是缺什么
      if (nodeIsTooOld(d)) {
        appendLog('launcher', t('wiz_node_old_log', d.node_version || '?', d.node_min_version || 'v22.19.0'));
      }
      // pnpm 同理：低于下限要更新（首装向导会把「完成」拦住，这里先把话说到位）
      if (pnpmIsTooOld(d)) {
        const [pcur, pmin] = pnpmVersionPair(d);
        appendLog('launcher', t('wiz_pnpm_old_log', pcur, pmin));
      }
      toast(d.node_found && d.npm_found && d.dsh_found && pnpmOk(d)
        ? t('toast_detect_full') : t('toast_detect_missing'));
    } catch (e) {
      toast(t('toast_detect_fail', e), true);
    }
  };

  // 开机自启开关（即时生效）
  $('set-autostart').onchange = async (ev) => {
    const enabled = ev.target.checked;
    try {
      await invoke('set_autostart', { enabled });
      toast(enabled ? t('toast_autostart_on') : t('toast_autostart_off'));
    } catch (err) {
      ev.target.checked = !enabled;
      toast(t('toast_autostart_fail', err), true);
    }
  };

  // ---- 首选项：Python 建议安装块（状态行由 openSettings 拉取，这里只管两个按钮）----
  $('btn-python-basic').onclick = () => runPythonInstall('basic');
  $('btn-python-extra').onclick = () => runPythonInstall('extra');

  // 检测全局包名
  $('btn-detect-package').onclick = async () => {
    try {
      const result = await invoke('detect_npm_package');
      appendLog('launcher', '[launcher] ' + result);
      toast(t('toast_pkg_done'));
    } catch (err) {
      appendLog('launcher', t('log_pkg_fail', err));
      toast(t('toast_pkg_fail', err), true);
    }
  };

  // ---- 首选项最底部「维护」：卸载全局 DSH 包（确认 → 执行 → 追问 pnpm → 结果页）----
  $('btn-uninstall-dsh').onclick = () => openUninstallConfirm();
  $('btn-uninstall-cancel').onclick = () => {
    if (uninstalling) return; // 正在删：取消按钮也禁用，避免半途关掉看不见结果
    hideModal('uninstall-modal');
  };
  $('btn-uninstall-confirm').onclick = () => doUninstallDsh();
  // 追问：答「是」= 顺带卸载 pnpm；答「否」= 直接进最终结果页（并给出日后的手动命令）
  $('btn-uninstall-pnpm-yes').onclick = () => doUninstallPnpm();
  $('btn-uninstall-pnpm-no').onclick = () => {
    if (uninstalling) return;
    showUninstallFinal(false);
  };
  $('btn-uninstall-final-close').onclick = () => hideModal('uninstall-pnpm-modal');

  // ---- 首选项：包源 registry 对齐（读自动刷、写要点按钮）----
  // 与 npm 缓存 / DSH_HOME 同款分工：后端返回一句已生成好的消息，成功进日志并弹 toast，
  // 失败单独报；两种结局都重问一次状态行 —— 按钮要不要还在，以刚发生的事实为准。
  $('btn-registry-align').onclick = async () => {
    try {
      const msg = await invoke('registry_align_apply');
      appendLog('launcher', '[launcher] ' + msg);
      toast(msg);
      refreshRegistryInfo();
    } catch (err) {
      toast(t('toast_registry_fail', err), true);
      appendLog('launcher', t('toast_registry_fail', err));
    }
  };

  $('btn-registry-restore').onclick = async () => {
    try {
      const msg = await invoke('registry_restore');
      appendLog('launcher', '[launcher] ' + msg);
      toast(msg);
      refreshRegistryInfo();
    } catch (err) {
      toast(t('toast_registry_fail', err), true);
      appendLog('launcher', t('toast_registry_fail', err));
    }
  };

  // 浏览按钮（文件/文件夹选择由 Rust 端 dialog 插件完成，结果经 path-picked 事件回填）
  document.querySelectorAll('[data-pick]').forEach((btn) => {
    btn.onclick = () => {
      const kind = btn.dataset.kind;
      const cmd = btn.dataset.pick === 'folder' ? 'pick_folder' : 'pick_exec_path';
      invoke(cmd, { kind }).catch((e) => toast(String(e), true));
    };
  });

  // 家目录：手输即判「与已保存的不同」，当场亮出/收起搬家警告。
  // 用 input 而不是 change：警告的作用是「动手拷贝之前就说清楚」，等到失焦就晚了半拍。
  const homeDirEl = $('set-home-dir');
  if (homeDirEl) homeDirEl.addEventListener('input', refreshHomeMoveWarn);

  // 「安装位置」：手输一次就标记为「用户动过」，之后检测结果不再覆盖它（见 prefillNodeDir）
  const nodeDirEl = $('wiz-node-dir');
  if (nodeDirEl) nodeDirEl.addEventListener('input', () => markDirTouched(nodeDirEl));
  const dshDirEl = $('wiz-dsh-dir');
  if (dshDirEl) {
    dshDirEl.addEventListener('input', () => {
      markDirTouched(dshDirEl);
      refreshDshCmd();
    });
  }

  // ---- 首次运行引导向导按钮 ----
  // 第一步语言选择：固定双语按钮（不挂 data-i18n，永不被词典改写）
  $('wiz-btn-lang-en').onclick = () => onWizLanguage('en');
  $('wiz-btn-lang-zh').onclick = () => onWizLanguage('zh');
  $('wiz-btn-recheck').onclick = () => wizDetect();
  $('wiz-btn-recheck2').onclick = () => wizDetect();
  $('wiz-btn-skip-node').onclick = () => { appendLog('launcher', t('wiz_skip_node_log')); wizFinish(); };
  $('wiz-btn-skip-dsh').onclick = () => { appendLog('launcher', t('wiz_skip_dsh_log')); wizFinish(); };
  $('wiz-btn-finish').onclick = () => wizFinish();

  // ---- Node.js 版本过低告警：三条路（一键升级 / 自行安装 / 保留并继续） ----
  $('wiz-btn-upgrade-node').onclick = () => runNodeLtsInstall();
  $('wiz-btn-node-recheck').onclick = () => wizDetect();
  $('wiz-btn-node-manual').onclick = () => {
    const page = (wiz.detection && wiz.detection.node_download_page) || 'https://nodejs.org/en/download';
    appendLog('launcher', t('wiz_node_manual_log'));
    invoke('open_in_browser', { url: page }).catch((e) => toast(String(e), true));
  };
  $('wiz-btn-node-ignore').onclick = async () => {
    const det = wiz.detection || lastEnvDetection;
    const version = det && det.node_version ? String(det.node_version) : '';
    try {
      config = await invoke('remember_node_min_version_notice', { version, ignore: true });
    } catch (e) {
      toast(t('toast_node_ack_fail', e), true);
      return;
    }
    appendLog('launcher', t('wiz_node_ignore_log'));
    toast(t('toast_node_ack_saved'));
    refreshNodeIndicators();
  };

  $('wiz-btn-copy-dsh-cmd').onclick = async () => {
    const ok = await copyText($('wiz-dsh-cmd').textContent);
    toast(ok ? t('wiz_cmd_copied') : t('toast_copy_fail'), !ok);
  };
  $('wiz-btn-install-node').onclick = async () => {
    if (wiz.busy) return;
    // 安装位置：留空 → 传 null → 后端不传 INSTALLDIR（官方默认目录）。
    // 只有这条「首次安装」路径开放换目录：「版本过低 → 升级」那条故意不开放，
    // 因为同一个 ProductCode 改 INSTALLDIR 会留下旧目录与旧的 PATH 条目。
    const dirEl = $('wiz-node-dir');
    const installDir = dirEl && dirEl.value ? dirEl.value.trim() : '';
    wiz.busy = true;
    renderWiz();
    setWizProgress(true, t('wiz_download_node'));
    $('wiz-log').classList.remove('hidden');
    try {
      await invoke('setup_install_node', { dir: installDir || null });
      // 进度与结果由 setup-status / setup-result 事件驱动（Rust 端按语言输出）
    } catch (e) {
      wiz.busy = false;
      setWizProgress(false);
      renderWiz();
      $('wiz-log').classList.remove('hidden');
      toast(String(e), true);
    }
  };
  $('wiz-btn-install-dsh').onclick = async () => {
    if (wiz.busy) return;
    // 安装位置：留空 → 传 null → 后端不传 --prefix（= npm 用自己的默认全局目录）。
    // 参数名同 Node 那条的约定：用**单词** dir，避免 Tauri 的 camelCase/snake_case
    // 转换把键名对不上时静默变成 None（= 悄悄装回默认目录）。
    const dirEl = $('wiz-dsh-dir');
    const installDir = dirEl && dirEl.value ? dirEl.value.trim() : '';
    wiz.busy = true;
    renderWiz();
    setWizProgress(true, t('wiz_install_dsh_progress'));
    $('wiz-log').classList.remove('hidden');
    try {
      await invoke('setup_install_dsh', { dir: installDir || null });
    } catch (e) {
      wiz.busy = false;
      setWizProgress(false);
      renderWiz();
      $('wiz-log').classList.remove('hidden');
      toast(String(e), true);
    }
  };
  // ---- 缺少 pnpm / pnpm 版本过低：一键安装（或更新） / 重新检测 ----
  // pnpm 是「DSH 装好之后安装插件」用的包管理器，本程序对它有硬要求（≥ v10）：
  // 缺失或过旧都会拦下「完成」按钮，所以这里从「建议」变成了**必经**的一步
  // （进度与结果由 setup-status / setup-result（target = "pnpm"）事件驱动；
  // 后端装完会重新核对版本，成功后 onSetupResult 自动刷新本页状态）。
  $('wiz-btn-install-pnpm').onclick = async () => {
    if (wiz.busy) return;
    wiz.busy = true;
    appendLog('launcher', t('log_pnpm_install_manual'));
    renderWiz();
    setWizProgress(true, t('wiz_install_pnpm_progress'));
    $('wiz-log').classList.remove('hidden');
    try {
      await invoke('setup_install_pnpm');
    } catch (e) {
      wiz.busy = false;
      setWizProgress(false);
      renderWiz();
      $('wiz-log').classList.remove('hidden');
      toast(String(e), true);
    }
  };
  $('wiz-btn-recheck-pnpm').onclick = () => wizDetect();
  // 官网链接用事件委托：applyDom 重写 html 后 <a> 会被重建，直接绑会丢
  $('setup-wizard').addEventListener('click', (ev) => {
    const a = ev.target.closest('a#wiz-open-node-page');
    if (!a) return;
    ev.preventDefault();
    const page = (wiz.detection && wiz.detection.node_download_page) || 'https://nodejs.org/en/download';
    invoke('open_in_browser', { url: page }).catch((e) => toast(String(e), true));
  });

  // 点击遮罩关闭模态框（误点弹窗外区域可直接关掉）
  MODALS.forEach((m) => {
    $(m).addEventListener('mousedown', (ev) => {
      if (ev.target === $(m)) hideModal(m);
    });
  });
}

window.addEventListener('DOMContentLoaded', () => {
  init().catch((e) => {
    appendLog('launcher', '[launcher] ' + (window.I18N ? t('init_fail', e) : e));
    toast(String(e), true);
  });
});
