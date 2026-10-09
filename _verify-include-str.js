// 校验 include_str! 声明的桥接源码与磁盘文件一一对应。
// 用法：node _verify-include-str.js   （在仓库根目录或任意目录跑都行，路径写死在下面）
const fs = require('fs');
const path = require('path');
const ROOT = 'D:/DSH/Workspace/DSH-Desktop';
const rs = fs.readFileSync(path.join(ROOT, 'src-tauri/src/py_bridge.rs'), 'utf8');

function check(resSub, pattern, note) {
  const declared = [...rs.matchAll(pattern)].map((m) => m[1]);
  const onDisk = fs.readdirSync(path.join(ROOT, 'src-tauri/resources', resSub))
    .filter((f) => /\.(py|json|yml|js)$/.test(f));
  const missing = declared.filter((f) => !onDisk.includes(f));
  const notDeclared = onDisk.filter((f) => !declared.includes(f));
  const ok = missing.length === 0 && notDeclared.length === 0;
  console.log((ok ? 'OK   ' : 'FAIL ') + note + `（声明 ${declared.length} / 磁盘 ${onDisk.length}）`);
  if (!ok) {
    if (missing.length) console.log('     声明了但磁盘没有:', missing.join(', '));
    if (notDeclared.length) console.log('     磁盘有但没声明:', notDeclared.join(', '));
  }
  return ok;
}

const a = check('py-bridge', /include_str!\("\.\.\/resources\/py-bridge\/([\w.]+)"\)/g, '桥接 Python 源码');
const b = check('py-bridge-bundle', /include_str!\("\.\.\/resources\/py-bridge-bundle\/([\w.\-]+)"\)/g, 'bundle 注册文件');
if (a && b) console.log('\n11 个 include_str! 全部与磁盘对应 —— 打出来的包不是空壳。');
else process.exit(1);
