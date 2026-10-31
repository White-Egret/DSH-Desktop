// 校验 `include_str!` 声明的桥接源码与磁盘文件一一对应。
//
// 为什么需要它：`src-tauri/src/py_bridge.rs` 用 `include_str!` 在**编译期**把 Python
// 桥接的源码嵌进二进制。这层机制有两个静默失效的方向：
//   - 声明了但磁盘没有 → 编译直接失败（还好，这个会报）；
//   - 磁盘有但没声明   → **编译不报错**，那个文件只是没被打进安装包，
//                        运行时桥接进程 import 阶段才崩，用户看到的是一句看不懂的错。
// 第二个方向只能靠「比对声明与磁盘」发现，所以这条检查是唯一能提前拦住它的东西。
//
// 用法（仓库根目录或本目录下都行）：
//   node scripts/verify-include-str.js
//   npm run verify:include-str
//
// 退出码：0 = 全部对应；1 = 有缺失或多余（可直接接进任何钩子）。
//
// 历史：本文件原先叫 `_verify-include-str.js` 躺在仓库根目录，`ROOT` 写死成
// `D:/DSH/Workspace/DSH-Desktop` —— 换台机器或换个目录就跑不了（与 .gitignore 里
// 忽略 `src-tauri/.cargo/` 是同一个理由：绝对路径只对一台机器成立）。搬到 scripts/
// 并改用 `__dirname` 推导后，它在任何位置、任何机器上都能跑。
const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.join(__dirname, '..');

/** 读 py_bridge.rs：它是 include_str! 声明的唯一来源 */
function readBridgeSource() {
  const file = path.join(ROOT, 'src-tauri', 'src', 'py_bridge.rs');
  return fs.readFileSync(file, 'utf8');
}

/**
 * 比对一组声明与磁盘文件。返回 true = 一一对应。
 *
 * `resSub` 是 `src-tauri/resources/` 下的子目录名；`pattern` 必须带一个捕获组，
 * 捕获的就是文件名（不含目录）。
 */
function check(rs, resSub, pattern, note) {
  const declared = [...rs.matchAll(pattern)].map((m) => m[1]);
  const dir = path.join(ROOT, 'src-tauri', 'resources', resSub);
  let onDisk;
  try {
    onDisk = fs.readdirSync(dir).filter((f) => /\.(py|json|yml|js)$/.test(f));
  } catch (_) {
    console.log(`FAIL ${note}：目录读不出来 ${dir}`);
    return false;
  }
  const missing = declared.filter((f) => !onDisk.includes(f));
  const notDeclared = onDisk.filter((f) => !declared.includes(f));
  const ok = missing.length === 0 && notDeclared.length === 0;
  console.log(
    (ok ? 'OK   ' : 'FAIL ') + note + `（声明 ${declared.length} / 磁盘 ${onDisk.length}）`,
  );
  if (!ok) {
    if (missing.length) console.log('     声明了但磁盘没有:', missing.join(', '));
    if (notDeclared.length) console.log('     磁盘有但没声明:', notDeclared.join(', '));
  }
  return ok;
}

// ---- 自测：确认正则真的抓得到声明，而不是「一条都没匹配到」的空转 ----
// 这正是 check-i18n-args.js 记下的教训：扫描器本身写歪时，什么都没扫到也会报 OK。
(function selfTest(rs) {
  const pyPattern = /include_str!\("\.\.\/resources\/py-bridge\/([\w.]+)"\)/g;
  const bundlePattern = /include_str!\("\.\.\/resources\/py-bridge-bundle\/([\w.\-]+)"\)/g;
  const pyHits = [...rs.matchAll(pyPattern)];
  const bundleHits = [...rs.matchAll(bundlePattern)];
  if (pyHits.length === 0 || bundleHits.length === 0) {
    console.log(
      `FAIL 自测：正则没匹配到任何声明（py-bridge ${pyHits.length} / bundle ${bundleHits.length}）` +
        ' —— 多半是 py_bridge.rs 里的 include_str! 写法变了，先修正则再信结果',
    );
    process.exit(1);
  }
})(readBridgeSource());

// ---- 真实比对 ----
const rs = readBridgeSource();
const a = check(
  rs,
  'py-bridge',
  /include_str!\("\.\.\/resources\/py-bridge\/([\w.]+)"\)/g,
  '桥接 Python 源码',
);
const b = check(
  rs,
  'py-bridge-bundle',
  /include_str!\("\.\.\/resources\/py-bridge-bundle\/([\w.\-]+)"\)/g,
  'bundle 注册文件',
);

if (a && b) {
  console.log('\n所有 include_str! 都与磁盘一一对应 —— 打出来的包不是空壳。');
  process.exit(0);
}
process.exit(1);
