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
let launchedByAutostart = false; // 本次进程是否由「开机自启」触发（决定静默+延迟策略）
let statusMessage = null;        // 最近一次状态事件携带的附加消息（Rust 端已本地化）
let lastErrorText = '';          // 最近一次错误文本（「复制错误信息」使用）

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
    if (!anyModalOpen()) invoke('set_dsh_webview_visible', { visible: true }).catch(() => {});
  }, 1600);

  invoke('refresh_dsh_page')
    .then(() => {
      // 页面正在重新加载；提示层由上面的定时器收起
    })
    .catch((err) => {
      clearTimeout(refreshTimer);
      ov.classList.add('hidden');
      if (!anyModalOpen()) invoke('set_dsh_webview_visible', { visible: true }).catch(() => {});
      toast(String(err), true);
    });
}

// F5 / Ctrl+R 快捷键：焦点在 Launcher 界面时触发刷新；
// DSH 页面内部 WebView2 自带 F5/Ctrl+R 刷新行为，两者效果一致。
window.addEventListener('keydown', (e) => {
  if (e.key === 'F5' || ((e.ctrlKey || e.metaKey) && (e.key === 'r' || e.key === 'R'))) {
    e.preventDefault();
    refreshPage();
  }
});

// ---------- 模态框（打开时隐藏内嵌 DSH webview，避免其盖住弹窗） ----------

const MODALS = ['settings-modal', 'log-modal', 'update-modal'];

function anyModalOpen() {
  return MODALS.some((m) => !$(m).classList.contains('hidden'));
}

function syncWebviewVisibility() {
  // 无 webview 时 Rust 端为 no-op，可安全调用
  invoke('set_dsh_webview_visible', { visible: !anyModalOpen() }).catch(() => {});
}

function showModal(id) {
  $(id).classList.remove('hidden');
  syncWebviewVisibility();
}

function hideModal(id) {
  $(id).classList.add('hidden');
  syncWebviewVisibility();
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

// ---------- 状态 UI ----------

function onStatus(p) {
  status = p.status;
  statusMessage = p.message || null; // 供 renderWaitLine 显示开机自启延迟等提示（Rust 端已本地化）
  const map = STATUS_META[p.status] || { key: null, dot: 'gray' };
  $('status-dot').className = 'dot ' + map.dot;
  $('status-text').textContent = map.key ? t(map.key) : p.status;
  $('port-val').textContent = p.port;

  const line = $('stage-line');
  const hint = $('stage-hint');
  const busyPanel = $('port-busy-panel');
  const showSpinner = ['starting', 'stopping', 'updating'].includes(p.status);

  $('spinner').classList.toggle('hidden', !showSpinner);
  line.classList.remove('error');
  hint.classList.add('hidden');
  busyPanel.classList.add('hidden');
  $('btn-copy-error').classList.add('hidden');

  switch (p.status) {
    case 'idle':
      line.textContent = t('stage_idle');
      break;
    case 'starting':
      startWaitTimer(); // 内部会渲染等待行
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

  if (p.status !== 'starting') stopWaitTimer();
  refreshButtons();
}

function refreshButtons() {
  const busy = updating || ['starting', 'stopping', 'updating'].includes(status);
  $('btn-start').disabled = busy || ['running', 'running-external'].includes(status);
  $('btn-stop').disabled = updating || ['idle', 'error', 'port-busy', 'stopping'].includes(status);
  $('btn-restart').disabled = busy || !['running', 'running-external'].includes(status);
  $('btn-update').disabled = busy || !config || !config.npm_exists;
  $('btn-check-update').disabled = updating || checkingVersion;
  $('btn-settings').disabled = updating;
  $('btn-connect').disabled = updating;
}

// ---------- 事件监听 + 初始化 ----------

async function init() {
  await listen('dsh-log', (e) => appendLog(e.payload.stream, e.payload.line));
  await listen('update-log', (e) => appendLog('update', e.payload.line));
  await listen('dsh-status', (e) => onStatus(e.payload));
  await listen('update-finished', (e) => onUpdateFinished(e.payload));
  await listen('path-picked', (e) => onPathPicked(e.payload));
  await listen('autostart-changed', (e) => {
    // 托盘菜单或设置开关切换了开机自启后同步 UI（line: "on" | "off"）
    $('set-autostart').checked = e.payload.line === 'on';
  });
  await listen('setup-status', (e) => onSetupStatus(e.payload));
  await listen('setup-result', (e) => onSetupResult(e.payload));

  await refreshConfig();
  // 应用界面语言（中英文），随后渲染的静态文案全部走词典
  I18N.setLang(config && config.language);
  I18N.applyDom();

  const st = await invoke('get_status');
  onStatus(st);

  bindUI();

  // 从托盘恢复窗口时刷新状态，并确保内嵌 DSH Webview 重新显示
  // （WebView2 在 hide→show 后偶发白屏，这里再触发一次重绘兜底）
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) {
      syncWebviewVisibility();
      invoke('get_status').then(onStatus).catch(() => {});
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

// ---------- 语言切换（保存后立即应用；重启 Desktop 同样生效） ----------

function applyLanguage(lang) {
  I18N.setLang(lang);
  I18N.applyDom();
  // 重新渲染依赖语言的动态区域
  invoke('get_status').then(onStatus).catch(() => {});
  if (wiz.active) renderWiz();
}

// ---------- 设置 ----------

function openSettings() {
  if (!config) return;
  $('set-npm-path').value = config.npm_path;
  $('set-dsh-path').value = config.dsh_path;
  $('set-home-dir').value = config.dsh_home_dir;
  $('set-port').value = config.port;
  $('set-timeout').value = config.health_timeout_secs;
  $('set-close-action').value = config.close_action === 'quit' ? 'quit' : 'tray';
  $('set-language').value = config.language === 'en' ? 'en' : 'zh';
  $('set-extra-args').value = config.extra_args;
  $('set-package-name').value = config.package_name;
  $('set-update-args').value = config.update_args;
  $('set-config-path').textContent = config.config_path;
  markFlag('npm-exists-flag', config.npm_exists);
  markFlag('dsh-exists-flag', config.dsh_exists);
  markFlag('home-exists-flag', config.home_exists);
  showModal('settings-modal');
}

function markFlag(id, ok) {
  const el = $(id);
  el.textContent = ok ? t('flag_exists') : t('flag_missing');
  el.className = 'flag ' + (ok ? 'ok' : 'bad');
}

async function saveSettings() {
  // 端口校验：必须是 1~65535 的数字（要求一.5）
  const port = parseInt($('set-port').value, 10);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    toast(t('toast_port_invalid'), true);
    return;
  }
  const timeout = parseInt($('set-timeout').value, 10);
  const cfg = {
    npm_path: $('set-npm-path').value.trim(),
    dsh_path: $('set-dsh-path').value.trim(),
    dsh_home_dir: $('set-home-dir').value.trim(),
    port,
    close_action: $('set-close-action').value === 'quit' ? 'quit' : 'tray',
    language: $('set-language').value === 'en' ? 'en' : 'zh',
    // 0 = 一直等待，是合法值，不能用 || 兜底
    health_timeout_secs: Number.isFinite(timeout) && timeout >= 0 ? timeout : 300,
    extra_args: $('set-extra-args').value.trim(),
    package_name: $('set-package-name').value.trim() || '@deepseek-ai/dsh',
    update_args: $('set-update-args').value.trim() || 'install -g @deepseek-ai/dsh@latest',
  };
  const langChanged = cfg.language !== I18N.lang;
  try {
    config = await invoke('save_config', { config: cfg });
    if (langChanged) {
      // 先切语言再刷新弹窗内文案（词典 + 静态标签）
      applyLanguage(cfg.language);
    }
    markFlag('npm-exists-flag', config.npm_exists);
    markFlag('dsh-exists-flag', config.dsh_exists);
    markFlag('home-exists-flag', config.home_exists);
    $('set-config-path').textContent = config.config_path;
    $('port-val').textContent = config.port;
    toast(t('toast_saved'));
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
    if (input) input.value = p.path;
  }
}

// ---------- 版本（显示在工具栏右侧：本地 → 最新） ----------

function renderVersion(local, latest) {
  const el = $('ver-val');
  if (local && latest) {
    el.textContent = local.trim() === latest.trim()
      ? t('ver_latest_suffix', local)
      : t('ver_arrow', local, latest);
  } else if (local) {
    el.textContent = local;
  } else if (latest) {
    el.textContent = t('ver_latest_only', latest);
  } else {
    el.textContent = t('ver_unknown2');
  }
}

async function checkVersions() {
  if (checkingVersion) return;
  checkingVersion = true;
  refreshButtons();
  $('ver-val').textContent = t('ver_querying');
  appendLog('launcher', t('log_ver_querying'));
  try {
    const info = await invoke('check_versions');
    if (info.local) appendLog('launcher', t('log_ver_local', info.local));
    if (info.latest) {
      appendLog('launcher', t('log_ver_latest', info.latest));
      if (info.local && info.local.trim() !== info.latest.trim()) {
        appendLog('launcher', t('log_ver_update_avail', info.local.trim(), info.latest.trim()));
      }
    }
    if (info.error) appendLog('launcher', t('log_ver_error', info.error));
    renderVersion(info.local, info.latest);
    if (info.error && !info.local && !info.latest) toast(info.error, true);
  } catch (err) {
    renderVersion(null, null);
    appendLog('launcher', t('log_ver_fail', err));
    toast(t('toast_ver_fail', err), true);
  } finally {
    checkingVersion = false;
    refreshButtons();
  }
}

// ---------- 更新 ----------

function confirmUpdate() {
  if (!config) return;
  $('update-cmd-preview').textContent = `cmd /C "${config.npm_path}" ${config.update_args}`;
  showModal('update-modal');
}

async function doUpdate() {
  hideModal('update-modal');
  if (updating) return;
  updating = true;
  refreshButtons();
  appendLog('update', t('log_update_begin'));
  try {
    await invoke('update_dsh');
  } catch (err) {
    updating = false;
    refreshButtons();
    toast(t('toast_update_fail_start', err), true);
    appendLog('update', t('log_update_start_fail', err));
  }
}

async function onUpdateFinished(p) {
  updating = false;
  refreshButtons();
  if (p.success) {
    appendLog('update', t('log_update_done_restart'));
    toast(t('toast_update_restarting'));
    try {
      await invoke('start_dsh');
      await checkVersions();
    } catch (err) {
      appendLog('launcher', t('log_restart_fail', err));
    }
  } else {
    toast(t('toast_update_failed_detail', p.message), true);
  }
}

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
  } catch (e) {
    toast(t('wiz_env_fail', e), true);
    wiz.detection = null;
  }
  setWizProgress(false);
  renderWiz();
}

function setWizFlag(flagId, pathId, found, detail) {
  const f = $(flagId);
  f.textContent = found ? t('wiz_installed') : t('wiz_notfound');
  f.className = 'flag ' + (found ? 'ok' : 'bad');
  $(pathId).textContent = detail || '';
}

function renderWiz() {
  const d = wiz.detection;
  if (!d) return;

  setWizFlag('wiz-node-flag', 'wiz-node-path', d.node_found,
    d.node_found ? (d.node_path + (d.node_version ? '（' + d.node_version + '）' : '')) : '');
  setWizFlag('wiz-npm-flag', 'wiz-npm-path', d.npm_found, d.npm_path);
  setWizFlag('wiz-dsh-flag', 'wiz-dsh-path', d.dsh_found, d.dsh_path);

  const allOK = d.node_found && d.npm_found && d.dsh_found;

  // Node 步骤：缺 node/npm 时显示
  $('wiz-step-node').classList.toggle('hidden', wiz.busy || d.node_found);
  // DSH 步骤：node+npm 就绪但缺 DSH 时显示
  $('wiz-step-dsh').classList.toggle('hidden', wiz.busy || !d.node_found || !d.npm_found || d.dsh_found);

  // 引导信息（wiz-node-url / wiz-dsh-cmd 可能在 applyDom 后被重建，这里重新赋值即可）
  const urlEl = $('wiz-node-url');
  if (urlEl) urlEl.textContent = d.node_msi_url || 'https://nodejs.org/en/download';
  const cmdEl = $('wiz-dsh-cmd');
  if (cmdEl) cmdEl.textContent = '"' + (d.npm_path || 'npm') + '" install -g @deepseek-ai/dsh';

  // 完成按钮：全部就绪 → 直接进入；有缺失 → 等同「跳过」
  const finishBtn = $('wiz-btn-finish');
  finishBtn.disabled = false;
  finishBtn.textContent = allOK ? t('wiz_all_ready') : t('wiz_skip_go');
  finishBtn.classList.remove('hidden');

  // 安装进行中禁用相关按钮
  ['wiz-btn-install-node', 'wiz-btn-recheck', 'wiz-btn-skip-node',
   'wiz-btn-install-dsh', 'wiz-btn-copy-dsh-cmd', 'wiz-btn-recheck2', 'wiz-btn-skip-dsh']
    .forEach((id) => { $(id).disabled = wiz.busy; });
}

function setWizProgress(show, text) {
  const el = $('wiz-progress');
  el.classList.toggle('hidden', !show);
  if (text) $('wiz-progress-text').textContent = text;
}

function onSetupStatus(p) {
  if (!wiz.active) return;
  if (p.phase === 'download' || p.phase === 'install' || p.phase === 'verify') {
    setWizProgress(true, p.message); // 文案由 Rust 端按当前语言生成
    $('wiz-log').classList.remove('hidden');
  }
}

function onSetupResult(p) {
  if (!wiz.active) return;
  wiz.busy = false;
  renderWiz();
  if (p.success) {
    setWizProgress(true, p.message);
    toast(p.message || t('wiz_installed'));
    setTimeout(() => wizDetect(), 600); // 成功后自动重新检测并进入下一步
  } else {
    setWizProgress(false);
    $('wiz-log').classList.remove('hidden');
    toast(p.message || t('wiz_node_start_fail'), true);
  }
}

function wizFinish() {
  invoke('finish_setup')
    .then(async (report) => {
      config = report;
      $('port-val').textContent = config.port;
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
  $('btn-refresh').onclick = refreshPage;
  $('btn-check-update').onclick = checkVersions;
  $('btn-update').onclick = confirmUpdate;
  $('btn-log').onclick = () => showModal('log-modal');
  $('btn-settings').onclick = openSettings;
  $('btn-cancel-settings').onclick = () => hideModal('settings-modal');
  $('btn-save-settings').onclick = saveSettings;
  $('btn-close-log').onclick = () => hideModal('log-modal');
  $('btn-clear-log').onclick = () => { logBody.innerHTML = ''; };
  $('btn-cancel-update').onclick = () => hideModal('update-modal');
  $('btn-confirm-update').onclick = doUpdate;
  $('btn-connect').onclick = () => invoke('connect_existing').catch((e) => toast(String(e), true));
  $('btn-change-port').onclick = openSettings;

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
      if (d.npm_path) $('set-npm-path').value = d.npm_path;
      if (d.dsh_path) $('set-dsh-path').value = d.dsh_path;
      markFlag('npm-exists-flag', d.npm_found);
      markFlag('dsh-exists-flag', d.dsh_found);
      appendLog('launcher', t('log_detect_done',
        d.node_path || t('wiz_notfound'),
        d.node_version ? '(' + d.node_version + ')' : '',
        d.npm_path || t('wiz_notfound'),
        d.dsh_path || t('wiz_notfound')));
      toast(d.node_found && d.npm_found && d.dsh_found ? t('toast_detect_full') : t('toast_detect_missing'));
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

  // 浏览按钮（文件/文件夹选择由 Rust 端 dialog 插件完成，结果经 path-picked 事件回填）
  document.querySelectorAll('[data-pick]').forEach((btn) => {
    btn.onclick = () => {
      const kind = btn.dataset.kind;
      const cmd = btn.dataset.pick === 'folder' ? 'pick_folder' : 'pick_exec_path';
      invoke(cmd, { kind }).catch((e) => toast(String(e), true));
    };
  });

  // ---- 首次运行引导向导按钮 ----
  // 第一步语言选择：固定双语按钮（不挂 data-i18n，永不被词典改写）
  $('wiz-btn-lang-en').onclick = () => onWizLanguage('en');
  $('wiz-btn-lang-zh').onclick = () => onWizLanguage('zh');
  $('wiz-btn-recheck').onclick = () => wizDetect();
  $('wiz-btn-recheck2').onclick = () => wizDetect();
  $('wiz-btn-skip-node').onclick = () => { appendLog('launcher', t('wiz_skip_node_log')); wizFinish(); };
  $('wiz-btn-skip-dsh').onclick = () => { appendLog('launcher', t('wiz_skip_dsh_log')); wizFinish(); };
  $('wiz-btn-finish').onclick = () => wizFinish();
  $('wiz-btn-copy-dsh-cmd').onclick = async () => {
    const ok = await copyText($('wiz-dsh-cmd').textContent);
    toast(ok ? t('wiz_cmd_copied') : t('toast_copy_fail'), !ok);
  };
  $('wiz-btn-install-node').onclick = async () => {
    if (wiz.busy) return;
    wiz.busy = true;
    renderWiz();
    setWizProgress(true, t('wiz_download_node'));
    $('wiz-log').classList.remove('hidden');
    try {
      await invoke('setup_install_node');
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
    wiz.busy = true;
    renderWiz();
    setWizProgress(true, t('wiz_install_dsh_progress'));
    $('wiz-log').classList.remove('hidden');
    try {
      await invoke('setup_install_dsh');
    } catch (e) {
      wiz.busy = false;
      setWizProgress(false);
      renderWiz();
      $('wiz-log').classList.remove('hidden');
      toast(String(e), true);
    }
  };
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
