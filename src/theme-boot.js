// 外观引导脚本：主脚本（main.js）要异步 invoke('get_config') 才知道用户配置，
// 这里先用上次解析出的主题（localStorage 缓存）同步设置 data-theme，
// 避免深色用户在窗口出现的一瞬「先白一下」。main.js 读到配置后一定会覆盖它，
// 所以 localStorage 只是防闪缓存，不是真相来源（真相在 config.appearance）。
// 注意 CSP：script-src 'self' 允许同目录外部脚本，但内联 <script> 会被拒绝，
// 故必须以独立文件引入。
(function () {
  try {
    if (localStorage.getItem('dsh-desktop-theme') === 'dark') {
      document.documentElement.dataset.theme = 'dark';
    }
  } catch (_) {
    /* localStorage 不可用（隐私模式等）：按默认浅色处理，主脚本随后纠正 */
  }
})();
