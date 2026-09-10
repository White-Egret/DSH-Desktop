// 从官方仓库取「与当前锁定 CLI 版本一致」的 NSIS 安装脚本模板，只改一处：
// 把 currentUser 模式的默认安装目录从 $LOCALAPPDATA\<产品名> 改成 $PROFILE\<产品名>
// （即 C:\Users\<用户名>\DSH Desktop），其余一字不动。
//
// 为什么需要它：Tauri 没有「默认安装目录」配置项（见 tauri-apps/tauri#11015），
// 官网给的唯一口子是 bundle.windows.nsis.template 自定义模板。而 NSIS 的默认目录是在
// installer.nsi 的 .onInit 里写死的（`StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"`，
// 在 MUI_PAGE_DIRECTORY 之前执行，所以 installer_hooks 的 PREINSTALL 太晚、改不到默认值）。
//
// 为什么用脚本生成而不是手抄一份：官方模板约 700 行、含大量 Handlebars 占位符与 NSIS
// 宏，手抄必错。脚本按 package-lock.json 里锁定的 CLI 版本取对应 tag 的模板，
// 保证「模板与 CLI 同版本」；升级 Tauri CLI 后重新跑一次即可刷新。
//
// 用法：node scripts/gen-nsis-template.mjs
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');

/** 从 package-lock.json 读锁定的 @tauri-apps/cli 版本（保证模板与 CLI 同源） */
function pinnedCliVersion() {
  const lock = JSON.parse(readFileSync(join(root, 'package-lock.json'), 'utf8'));
  const entry = lock.packages && lock.packages['node_modules/@tauri-apps/cli'];
  if (entry && entry.version) return entry.version;
  const dep = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
  const raw = (dep.devDependencies && dep.devDependencies['@tauri-apps/cli']) || '';
  const m = raw.match(/(\d+\.\d+\.\d+)/);
  if (!m) throw new Error('无法从 package-lock.json / package.json 确定 @tauri-apps/cli 版本');
  return m[1];
}

/** 官方模板在同一仓库的 tauri-bundler crate 里 */
function templateUrl(version) {
  const tag = `tauri-cli-v${version}`;
  return `https://raw.githubusercontent.com/tauri-apps/tauri/refs/tags/${tag}/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi`;
}

const REQUIRED_VARS = [
  '{{product_name}}',
  '{{install_mode}}',
  '{{main_binary_name}}',
  '{{main_binary_path}}',
  '{{#each languages}}',
  'PLACEHOLDER_INSTALL_DIR',
];

const version = pinnedCliVersion();
const url = templateUrl(version);
console.log(`@tauri-apps/cli ${version} → ${url}`);

const res = await fetch(url);
if (!res.ok) {
  throw new Error(`下载官方模板失败（HTTP ${res.status}）。请检查网络，或确认 tag tauri-cli-v${version} 是否存在。`);
}
let text = await res.text();
text = text.replace(/^\uFEFF/, '');

for (const v of REQUIRED_VARS) {
  if (!text.includes(v)) {
    throw new Error(`模板缺少预期占位符 ${v}：官方模板结构可能已变，请人工核对后再改。`);
  }
}

const OLD_LINE = '      StrCpy $INSTDIR "$LOCALAPPDATA\\${PRODUCTNAME}"';
const NEW_LINE = [
  '      ; [DSH-Desktop 本地改动] 默认安装目录：$LOCALAPPDATA\\<产品名> → $PROFILE\\<产品名>',
  '      ; 即 C:\\Users\\<用户名>\\DSH Desktop（用户仍可在「选择安装位置」页改成任意目录）。',
  '      ; 已装过的用户不受影响：下面 RestorePreviousInstallLocation 会沿用注册表里记住的旧路径。',
  '      StrCpy $INSTDIR "$PROFILE\\${PRODUCTNAME}"',
].join('\n');

if (text.includes(NEW_LINE)) {
  console.log('模板已经是打过补丁的版本（幂等，无需再改）。');
} else {
  const count = text.split(OLD_LINE).length - 1;
  if (count !== 1) {
    throw new Error(`预期模板中恰好出现 1 次默认目录赋值，实际 ${count} 次；官方模板已变，请人工核对。`);
  }
  text = text.replace(OLD_LINE, NEW_LINE);
  console.log('已把 currentUser 默认安装目录改为 $PROFILE\\${PRODUCTNAME}。');
}

const out = join(root, 'src-tauri', 'nsis', 'installer.nsi');
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, text, 'utf8');
console.log(`已写出：${out}（${text.length} 字符）`);
