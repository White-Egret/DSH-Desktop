// DSH Launcher 前端（通过 window.__TAURI__ 全局 API 与 Rust 后端通信）
// 布局：43.2px 工具栏（按钮居左 / 状态一行居右）；工具栏下方在「等待/错误」提示
// 与内嵌 DSH 页面（native webview，盖在本页面之上）之间切换。
const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

let config = null;        // ConfigReport（含 exists 标志 + 展平的 config 字段）
let status = 'idle';      // idle|starting|running|running-external|stopping|error|port-busy|updating
let updating = false;
let checkingVersion = false;
let launchedByAutostart = false; // 本次进程是否由「开机自启」触发（决定静默+延迟策略）
let statusMessage = null;        // 最近一次状态事件携带的附加消息（如开机自启延迟提示）

const STATUS_MAP = {
  'idle':             { text: '未运行',           dot: 'gray' },
  'starting':         { text: '正在启动',          dot: 'yellow' },
  'running':          { text: '运行中',            dot: 'green' },
  'running-external': { text: '运行中（外部）',     dot: 'blue' },
  'stopping':         { text: '正在停止',          dot: 'yellow' },
  'error':            { text: '启动失败',          dot: 'red' },
  'port-busy':        { text: '端口被占用',        dot: 'orange' },
  'updating':         { text: '正在更新',          dot: 'purple' },
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
  const tail = timeout > 0 ? `（最长等待 ${timeout} 秒）` : '（一直等待直到就绪）';
  const prefix = statusMessage ? `${statusMessage} — ` : '';
  $('stage-line').textContent = `${prefix}已等待 ${secs} 秒…${tail}`;
}

function stopWaitTimer() {
  if (waitTimer) clearInterval(waitTimer);
  waitTimer = null;
}

// ---------- 状态 UI ----------

function onStatus(p) {
  status = p.status;
  statusMessage = p.message || null; // 供 renderWaitLine 显示开机自启延迟等提示
  const map = STATUS_MAP[p.status] || { text: p.status, dot: 'gray' };
  $('status-dot').className = 'dot ' + map.dot;
  $('status-text').textContent = map.text;
  $('port-val').textContent = p.port;

  const line = $('stage-line');
  const hint = $('stage-hint');
  const busyPanel = $('port-busy-panel');
  const showSpinner = ['starting', 'stopping', 'updating'].includes(p.status);

  $('spinner').classList.toggle('hidden', !showSpinner);
  line.classList.remove('error');
  hint.classList.add('hidden');
  busyPanel.classList.add('hidden');

  switch (p.status) {
    case 'idle':
      line.textContent = 'DSH 未运行 — 点击左上角「启动」开始';
      break;
    case 'starting':
      startWaitTimer(); // 内部会渲染等待行
      hint.textContent = 'DSH 冷启动（尤其重启电脑后首次）可能需要一两分钟，请耐心等待；点击「日志」可查看实时输出';
      hint.classList.remove('hidden');
      break;
    case 'running':
      line.textContent = 'DSH 已就绪，页面即将显示';
      break;
    case 'running-external':
      line.textContent = '已连接到现有服务（非本程序启动，关闭本程序不会停止它）';
      break;
    case 'stopping':
      line.textContent = '正在停止 DSH…';
      break;
    case 'updating':
      line.textContent = '正在更新 DSH，请勿关闭程序…';
      hint.textContent = '更新输出会实时写入「日志」（来源标记为 update）';
      hint.classList.remove('hidden');
      break;
    case 'error':
      line.textContent = p.message || '启动失败，详见日志';
      line.classList.add('error');
      hint.textContent = '点击「日志」查看 DSH 的完整输出；也可点击「启动」重试';
      hint.classList.remove('hidden');
      break;
    case 'port-busy':
      line.textContent = `端口 ${p.port} 已被占用`;
      busyPanel.classList.remove('hidden');
      $('busy-port').textContent = p.port;
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

  await refreshConfig();
  const st = await invoke('get_status');
  onStatus(st);

  bindUI();
  await refreshAutostartToggle();

  // 本次是否由「开机自启」触发（决定静默窗口 + 12 秒延迟启动）
  try {
    launchedByAutostart = await invoke('was_launched_by_autostart');
  } catch (_) { /* 保持 false */ }
  if (launchedByAutostart) {
    appendLog('launcher', '[launcher] 本次由开机自启触发：窗口保持隐藏，DSH 将延迟 12 秒启动（错开系统冷启动高峰）。点击托盘图标可显示窗口。');
  }

  // 从托盘恢复窗口时刷新状态，并确保内嵌 DSH Webview 重新显示
  // （WebView2 在 hide→show 后偶发白屏，这里再触发一次重绘兜底）
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) {
      syncWebviewVisibility();
      invoke('get_status').then(onStatus).catch(() => {});
    }
  });

  // 自动启动：DSH 路径有效时，程序启动即拉起服务（含上次超时失败的 error 状态）；
  // 若为开机自启，Rust 端 start_internal 会先延迟 12 秒
  if (config && config.dsh_exists && ['idle', 'error'].includes(status)) {
    appendLog('launcher', '[launcher] 程序已启动，正在自动启动 DSH 服务…');
    try {
      await invoke('start_dsh');
    } catch (err) {
      appendLog('launcher', '[launcher] 自动启动失败: ' + err);
    }
  } else if (config && !config.dsh_exists) {
    appendLog('launcher', '[launcher] 未找到 dsh.cmd（' + config.dsh_path + '），请在设置中手动选择路径。');
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

// ---------- 设置 ----------

function openSettings() {
  if (!config) return;
  $('set-npm-path').value = config.npm_path;
  $('set-dsh-path').value = config.dsh_path;
  $('set-home-dir').value = config.dsh_home_dir;
  $('set-port').value = config.port;
  $('set-timeout').value = config.health_timeout_secs;
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
  el.textContent = ok ? '✔ 存在' : '✘ 未找到';
  el.className = 'flag ' + (ok ? 'ok' : 'bad');
}

async function saveSettings() {
  const port = parseInt($('set-port').value, 10);
  const timeout = parseInt($('set-timeout').value, 10);
  const cfg = {
    npm_path: $('set-npm-path').value.trim(),
    dsh_path: $('set-dsh-path').value.trim(),
    dsh_home_dir: $('set-home-dir').value.trim(),
    port: Number.isFinite(port) && port > 0 ? port : 3000,
    // 0 = 一直等待，是合法值，不能用 || 兜底
    health_timeout_secs: Number.isFinite(timeout) && timeout >= 0 ? timeout : 300,
    extra_args: $('set-extra-args').value.trim(),
    package_name: $('set-package-name').value.trim() || '@deepseek-ai/dsh',
    update_args: $('set-update-args').value.trim() || 'install -g @deepseek-ai/dsh@latest',
  };
  try {
    config = await invoke('save_config', { config: cfg });
    markFlag('npm-exists-flag', config.npm_exists);
    markFlag('dsh-exists-flag', config.dsh_exists);
    markFlag('home-exists-flag', config.home_exists);
    $('set-config-path').textContent = config.config_path;
    $('port-val').textContent = config.port;
    toast('配置已保存');
    refreshButtons();
    hideModal('settings-modal');
    const st = await invoke('get_status');
    onStatus(st);
  } catch (err) {
    toast('保存失败: ' + err, true);
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
    el.textContent = local.trim() === latest.trim() ? `${local}（最新）` : `${local} → ${latest}`;
  } else if (local) {
    el.textContent = local;
  } else if (latest) {
    el.textContent = `最新 ${latest}`;
  } else {
    el.textContent = '未知';
  }
}

async function checkVersions() {
  if (checkingVersion) return;
  checkingVersion = true;
  refreshButtons();
  $('ver-val').textContent = '查询中…';
  appendLog('launcher', '[launcher] 正在查询 DSH 版本（本地 --version，远程 npm view）…');
  try {
    const info = await invoke('check_versions');
    if (info.local) appendLog('launcher', '[launcher] 本地 DSH 版本: ' + info.local);
    if (info.latest) {
      appendLog('launcher', '[launcher] 最新 DSH 版本: ' + info.latest);
      if (info.local && info.local.trim() !== info.latest.trim()) {
        appendLog('launcher', '[launcher] 有可用更新: ' + info.local.trim() + ' → ' + info.latest.trim());
      }
    }
    if (info.error) appendLog('launcher', '[launcher] 版本检测提示: ' + info.error);
    renderVersion(info.local, info.latest);
    if (info.error && !info.local && !info.latest) toast(info.error, true);
  } catch (err) {
    renderVersion(null, null);
    appendLog('launcher', '[launcher] 版本查询失败: ' + err);
    toast('版本查询失败: ' + err, true);
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
  appendLog('update', '[update] 开始更新 DSH（更新前已停止当前服务）…');
  try {
    await invoke('update_dsh');
  } catch (err) {
    updating = false;
    refreshButtons();
    toast('更新启动失败: ' + err, true);
    appendLog('update', '[update] 更新启动失败: ' + err);
  }
}

async function onUpdateFinished(p) {
  updating = false;
  refreshButtons();
  if (p.success) {
    appendLog('update', '[update] 更新完成，正在自动重新启动 DSH…');
    toast('DSH 更新成功，正在重新启动…');
    try {
      await invoke('start_dsh');
      await checkVersions();
    } catch (err) {
      appendLog('launcher', '[launcher] 重新启动 DSH 失败: ' + err);
    }
  } else {
    toast('更新失败: ' + p.message + '（详见日志，未启动 DSH）', true);
  }
}

// ---------- 绑定 ----------

function bindUI() {
  $('btn-start').onclick = () => invoke('start_dsh').catch((e) => toast(String(e), true));
  $('btn-stop').onclick = () => invoke('stop_dsh').catch((e) => toast(String(e), true));
  $('btn-restart').onclick = () => invoke('restart_dsh').catch((e) => toast(String(e), true));
  $('btn-check-update').onclick = checkVersions;
  $('btn-update').onclick = confirmUpdate;
  $('btn-log').onclick = () => {
    showModal('log-modal');
    if ($('chk-autoscroll').checked) logBody.scrollTop = logBody.scrollHeight;
  };
  $('btn-connect').onclick = () => invoke('connect_existing').catch((e) => toast(String(e), true));
  $('btn-change-port').onclick = openSettings;

  $('btn-settings').onclick = () => { openSettings(); refreshAutostartToggle(); };
  $('btn-cancel-settings').onclick = () => hideModal('settings-modal');
  $('btn-save-settings').onclick = saveSettings;

  // 开机自启开关：即时生效（写入系统注册表），不随「保存」按钮
  $('set-autostart').onchange = async (e) => {
    const target = e.target.checked;
    try {
      await invoke('set_autostart', { enabled: target });
      toast(target ? '已开启开机自动启动' : '已关闭开机自动启动');
    } catch (err) {
      toast('修改开机自启失败: ' + err, true);
      e.target.checked = !target; // 回滚
    }
  };

  $('btn-cancel-update').onclick = () => hideModal('update-modal');
  $('btn-confirm-update').onclick = doUpdate;

  $('btn-clear-log').onclick = () => { logBody.innerHTML = ''; };
  $('btn-close-log').onclick = () => hideModal('log-modal');
  $('chk-autoscroll').onchange = () => {
    if ($('chk-autoscroll').checked) logBody.scrollTop = logBody.scrollHeight;
  };
  $('btn-detect-package').onclick = async () => {
    appendLog('launcher', '[launcher] 正在执行 npm list -g --depth=0 …');
    try {
      const text = await invoke('detect_npm_package');
      text.split('\n').forEach((l) => appendLog('launcher', l));
      toast('检测结果已写入日志');
    } catch (err) {
      appendLog('launcher', '[launcher] 检测失败: ' + err);
      toast('检测失败: ' + err, true);
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

  // 点击遮罩关闭模态框（误点弹窗外区域可直接关掉）
  MODALS.forEach((m) => {
    $(m).addEventListener('mousedown', (ev) => {
      if (ev.target === $(m)) hideModal(m);
    });
  });
}

window.addEventListener('DOMContentLoaded', () => {
  init().catch((e) => {
    appendLog('launcher', '[launcher] 初始化失败: ' + e);
    toast('初始化失败: ' + e, true);
  });
});
