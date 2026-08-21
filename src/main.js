// DSH Launcher 前端控制台（通过 window.__TAURI__ 全局 API 与 Rust 后端通信）
const invoke = window.__TAURI__.core.invoke;
const listen = window.__TAURI__.event.listen;

const $ = (id) => document.getElementById(id);

let config = null;       // ConfigReport（含 exists 标志）
let status = 'idle';     // idle|starting|running|running-external|stopping|error|port-busy|updating
let updating = false;
let checkingVersion = false;

const STATUS_MAP = {
  'idle':             { text: '未运行',             dot: 'gray' },
  'starting':         { text: '正在启动 DSH…',       dot: 'yellow' },
  'running':          { text: '运行中',              dot: 'green' },
  'running-external': { text: '运行中（外部进程）',   dot: 'blue' },
  'stopping':         { text: '正在停止…',           dot: 'yellow' },
  'error':            { text: '错误',                dot: 'red' },
  'port-busy':        { text: '端口被占用',          dot: 'orange' },
  'updating':         { text: '正在更新 DSH…',        dot: 'purple' },
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

// ---------- 状态 UI ----------

function onStatus(p) {
  status = p.status;
  const map = STATUS_MAP[p.status] || { text: p.status, dot: 'gray' };
  $('status-dot').className = 'dot ' + map.dot;
  $('status-text').textContent = map.text;
  $('spinner').classList.toggle('hidden', !['starting', 'stopping', 'updating'].includes(p.status));
  $('pid-chip').classList.toggle('hidden', !p.pid);
  if (p.pid) $('pid-chip').textContent = 'PID ' + p.pid;
  $('port-val').textContent = p.port;

  const msg = $('status-msg');
  if (p.message) {
    msg.textContent = p.message;
    msg.classList.remove('hidden');
  } else {
    msg.classList.add('hidden');
  }

  $('port-busy-panel').classList.toggle('hidden', p.status !== 'port-busy');
  if (p.status === 'port-busy') $('busy-port').textContent = p.port;

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
  $('btn-open-dsh').disabled = !['running', 'running-external'].includes(status);
  $('btn-connect').disabled = updating;
}

// ---------- 事件监听 ----------

async function init() {
  await listen('dsh-log', (e) => appendLog(e.payload.stream, e.payload.line));
  await listen('update-log', (e) => appendLog('update', e.payload.line));
  await listen('dsh-status', (e) => onStatus(e.payload));
  await listen('update-finished', (e) => onUpdateFinished(e.payload));
  await listen('path-picked', (e) => onPathPicked(e.payload));

  await refreshConfig();
  const st = await invoke('get_status');
  onStatus(st);

  bindUI();

  // 自动启动：DSH 路径有效时，程序启动即拉起服务
  if (config && config.dsh_exists && ['idle', 'error'].includes(status)) {
    appendLog('launcher', '[launcher] 程序已启动，正在自动启动 DSH 服务…');
    try {
      await invoke('start_dsh');
    } catch (err) {
      // 错误已通过状态事件展示，这里补一条日志
      appendLog('launcher', '[launcher] 自动启动失败: ' + err);
    }
  } else if (config && !config.dsh_exists) {
    appendLog('launcher', '[launcher] 未找到 dsh.cmd（' + config.dsh_path + '），请点击「设置」手动选择路径。');
    openSettings();
  }
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
  $('set-home-dir').value = config.home_dir;
  $('set-port').value = config.port;
  $('set-timeout').value = config.health_timeout_secs;
  $('set-extra-args').value = config.extra_args;
  $('set-package-name').value = config.package_name;
  $('set-update-args').value = config.update_args;
  $('set-config-path').textContent = config.config_path;
  markFlag('npm-exists-flag', config.npm_exists);
  markFlag('dsh-exists-flag', config.dsh_exists);
  markFlag('home-exists-flag', config.home_exists);
  $('settings-modal').classList.remove('hidden');
}

function markFlag(id, ok) {
  const el = $(id);
  el.textContent = ok ? '✔ 存在' : '✘ 未找到';
  el.className = 'flag ' + (ok ? 'ok' : 'bad');
}

async function saveSettings() {
  const cfg = {
    npm_path: $('set-npm-path').value.trim(),
    dsh_path: $('set-dsh-path').value.trim(),
    home_dir: $('set-home-dir').value.trim(),
    port: parseInt($('set-port').value, 10) || 3000,
    health_timeout_secs: parseInt($('set-timeout').value, 10) || 30,
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
    $('settings-modal').classList.add('hidden');
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

// ---------- 版本 ----------

async function checkVersions() {
  if (checkingVersion) return;
  checkingVersion = true;
  refreshButtons();
  $('local-ver').textContent = '查询中…';
  $('latest-ver').textContent = '查询中…';
  appendLog('launcher', '[launcher] 正在查询 DSH 版本（本地 --version，远程 npm view）…');
  try {
    const info = await invoke('check_versions');
    if (info.local) {
      $('local-ver').textContent = info.local;
      appendLog('launcher', '[launcher] 本地 DSH 版本: ' + info.local);
    } else {
      $('local-ver').textContent = '未知';
    }
    if (info.latest) {
      $('latest-ver').textContent = info.latest;
      appendLog('launcher', '[launcher] 最新 DSH 版本: ' + info.latest);
      if (info.local && info.local.trim() !== info.latest.trim()) {
        appendLog('launcher', '[launcher] 有可用更新: ' + info.local.trim() + ' → ' + info.latest.trim());
      }
    } else {
      $('latest-ver').textContent = '未知';
    }
    if (info.error) appendLog('launcher', '[launcher] 版本检测提示: ' + info.error);
  } catch (err) {
    $('local-ver').textContent = '未知';
    $('latest-ver').textContent = '未知';
    appendLog('launcher', '[launcher] 版本查询失败: ' + err);
  } finally {
    checkingVersion = false;
    refreshButtons();
  }
}

// ---------- 更新 ----------

function confirmUpdate() {
  if (!config) return;
  $('update-cmd-preview').textContent = `cmd /C "${config.npm_path}" ${config.update_args}`;
  $('update-modal').classList.remove('hidden');
}

async function doUpdate() {
  $('update-modal').classList.add('hidden');
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
      await invoke('check_versions');
    } catch (err) {
      appendLog('launcher', '[launcher] 重启 DSH 失败: ' + err);
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
  $('btn-open-dsh').onclick = () => invoke('open_dsh_window').catch((e) => toast(String(e), true));
  $('btn-connect').onclick = () => invoke('connect_existing').catch((e) => toast(String(e), true));
  $('btn-change-port').onclick = openSettings;

  $('btn-settings').onclick = openSettings;
  $('btn-cancel-settings').onclick = () => $('settings-modal').classList.add('hidden');
  $('btn-save-settings').onclick = saveSettings;

  $('btn-cancel-update').onclick = () => $('update-modal').classList.add('hidden');
  $('btn-confirm-update').onclick = doUpdate;

  $('btn-clear-log').onclick = () => { logBody.innerHTML = ''; };
  $('btn-detect-package').onclick = async () => {
    appendLog('launcher', '[launcher] 正在执行 npm list -g --depth=0 …');
    try {
      const text = await invoke('detect_npm_package');
      text.split('\n').forEach((l) => appendLog('launcher', l));
    } catch (err) {
      appendLog('launcher', '[launcher] 检测失败: ' + err);
    }
  };

  // 浏览按钮（文件/文件夹选择由 Rust 端 dialog 插件完成）
  document.querySelectorAll('[data-pick]').forEach((btn) => {
    btn.onclick = () => {
      const kind = btn.dataset.kind;
      const cmd = btn.dataset.pick === 'folder' ? 'pick_folder' : 'pick_exec_path';
      invoke(cmd, { kind }).catch((e) => toast(String(e), true));
    };
  });
}

window.addEventListener('DOMContentLoaded', () => {
  init().catch((e) => {
    appendLog('launcher', '[launcher] 初始化失败: ' + e);
  });
});
