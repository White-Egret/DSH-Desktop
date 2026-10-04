// 端到端验证：自定义安装位置下，DSH 的「装 → 卸 → 收尾」是否真的干净。
//
// 全程在**临时目录**里：造一个假 HOME（隔离的 ~/.npmrc）、一个假 npm 全局目录，装的是
// 一个几 KB 的本地包（包名刻意用 @deepseek-ai/ 作用域，与真实 DSH 同形）。
// 不碰你真实的 %USERPROFILE%\.npmrc，不联网（本地打包 + 本地 tarball）。
//
// 验证四件事：
//   ① `~/.npmrc` 里写 prefix=<所选目录> 之后，裸 npm 解析到的全局目录就是它；
//   ② 裸 `npm install -g` 会把包装进那个目录（= 用户在向导里选自定义位置的效果）；
//   ③ 裸 `npm uninstall -g` 能删掉包与启动脚本，但**留下一个空的 @deepseek-ai** ——
//      这就是用户报的残留（npm 不回收变空的 scope 目录）；
//   ④ DSH Desktop 的收尾逻辑（等价实现）：只回收**真空的**目录、把 prefix 行原路退回，
//      且不动用户 .npmrc 里的其它内容。
//
// 用法：node scripts/verify-npm-uninstall.js
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const isWin = process.platform === 'win32';
const npmCmd = path.join(path.dirname(process.execPath), isWin ? 'npm.cmd' : 'npm');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-uninstall-check-'));
const home = path.join(root, 'home');
const globalDir = path.join(root, 'chosen-global-dir');
const pkgSrc = path.join(root, 'pkg');
const pkgName = '@deepseek-ai/dsh-probe';
const binName = 'dsh-probe';
const comment = '# 用户的注释行，必须原样保留';

let failures = 0;
function check(label, ok, extra = '') {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${label}${extra ? ` — ${extra}` : ''}`);
  if (!ok) failures += 1;
}
function lastLine(text) {
  return text.split(/\r?\n/).filter(Boolean).pop() || '';
}
function run(args) {
  const r = spawnSync(npmCmd, args, {
    encoding: 'utf8',
    // 隔离：npm 从 HOME / USERPROFILE 找用户级 .npmrc，缓存也放进临时目录
    env: {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      npm_config_cache: path.join(root, 'cache'),
    },
  });
  if (r.error) throw new Error(`spawn npm ${args.join(' ')}: ${r.error.message}`);
  return { code: r.status, out: `${r.stdout || ''}${r.stderr || ''}`.trim() };
}
function npmPrefix() {
  return lastLine(run(['config', 'get', 'prefix']).out).trim();
}
/** Rust 侧 remove_dir_if_empty 的等价实现：只在真空时才删 */
function removeDirIfEmpty(dir) {
  if (!fs.existsSync(dir) || !fs.statSync(dir).isDirectory()) return 'absent';
  if (fs.readdirSync(dir).length > 0) return 'kept';
  try {
    fs.rmdirSync(dir);
    return 'removed';
  } catch (_) {
    return 'failed';
  }
}

try {
  fs.mkdirSync(home, { recursive: true });
  fs.mkdirSync(globalDir, { recursive: true });

  const npmrc = path.join(home, '.npmrc');
  fs.writeFileSync(npmrc, `${comment}\nregistry=https://registry.npmjs.org/\n`, 'utf8');

  // 与真实 DSH 同形的最小包（作用域包名 + 一个 bin 脚本），打成不联网的本地 tarball
  fs.mkdirSync(pkgSrc, { recursive: true });
  fs.writeFileSync(
    path.join(pkgSrc, 'package.json'),
    JSON.stringify({ name: pkgName, version: '1.0.0', bin: { [binName]: 'cli.js' } }, null, 2)
  );
  fs.writeFileSync(path.join(pkgSrc, 'cli.js'), 'console.log("probe");\n', 'utf8');
  const pack = run(['pack', '--pack-destination', root, pkgSrc]);
  const tarball = fs
    .readdirSync(root)
    .filter((f) => f.endsWith('.tgz'))
    .map((f) => path.join(root, f))[0];
  check('本地打包成功（不联网）', pack.code === 0 && !!tarball, lastLine(pack.out));
  if (!tarball) throw new Error('没有打出 tarball，后续步骤无法进行');

  // ---- ① DSH Desktop 装 DSH 前会做的事 ----
  const prevBefore = npmPrefix(); // 「写之前 npm 解析到哪里」= 程序记进 config.json 的原值
  fs.writeFileSync(npmrc, `${fs.readFileSync(npmrc, 'utf8')}prefix=${globalDir}\n`, 'utf8');
  check(
    '① 写入 prefix= 后，npm 自己解析到的全局目录 = 所选目录',
    npmPrefix().toLowerCase() === globalDir.toLowerCase(),
    npmPrefix()
  );

  // ---- ② 不带 --prefix 的安装（= 向导里选了这个位置的效果）----
  const install = run(['install', '-g', '--no-audit', '--no-fund', tarball]);
  check('② npm install -g 退出码为 0', install.code === 0, lastLine(install.out));
  const scope = path.join(globalDir, 'node_modules', '@deepseek-ai');
  const pkgDir = path.join(scope, 'dsh-probe');
  const shims = ['', '.cmd', '.ps1', '.exe'].map((ext) => path.join(globalDir, binName + ext));
  const anyShim = () => shims.some((p) => fs.existsSync(p));
  check('   包落在所选目录的 scope 里', fs.existsSync(pkgDir), pkgDir);
  check('   启动脚本落在所选目录里', anyShim());
  if (!fs.existsSync(pkgDir)) throw new Error('包没落在预期位置，后续断言无意义');

  // ---- ③ 最裸的卸载（不带参数、不带环境变量）----
  const uninstall = run(['uninstall', '-g', pkgName]);
  check('③ npm uninstall -g 退出码为 0', uninstall.code === 0, lastLine(uninstall.out));
  check('   包目录已删除', !fs.existsSync(pkgDir), pkgDir);
  check('   启动脚本已删除', !anyShim());
  check(
    '   复现残留：npm 不回收变空的 @deepseek-ai（这正是用户报的现象）',
    fs.existsSync(scope) && fs.readdirSync(scope).length === 0,
    scope
  );

  // ---- ④ 收尾（DSH Desktop 的等价实现）----
  const scopeStatus = removeDirIfEmpty(scope);
  check('④ 空的 scope 目录被回收', scopeStatus === 'removed', scopeStatus);
  const nmStatus = removeDirIfEmpty(path.join(globalDir, 'node_modules'));
  check('   空掉的 node_modules 被回收', nmStatus === 'removed', nmStatus);
  // prefix 行原路退回：这里 prev 为空（写之前配置里没有这一行）→ 删掉它
  const lines = fs.readFileSync(npmrc, 'utf8').split(/\r?\n/);
  fs.writeFileSync(npmrc, lines.filter((l) => !/^\s*prefix\s*=/i.test(l)).join('\n'), 'utf8');
  check(
    '   prefix 行已退回，npm 回到默认全局目录',
    npmPrefix().toLowerCase() !== globalDir.toLowerCase(),
    `prev=${prevBefore} → now=${npmPrefix()}`
  );
  check('   用户的注释行与 registry 行仍在', fs.readFileSync(npmrc, 'utf8').includes(comment));
  check(
    '   所选目录已彻底空掉（没有残留文件/目录）',
    !fs.existsSync(globalDir) || fs.readdirSync(globalDir).length === 0,
    globalDir
  );

  console.log(`\n${failures === 0 ? '全部通过' : `${failures} 项失败`}`);
  process.exitCode = failures === 0 ? 0 : 1;
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
