// 端到端验证：DSH Desktop 改完之后，「裸 npm uninstall -g <包名>」是否真能卸干净。
//
// 全程在**临时目录**里：造一个假 HOME（隔离的 ~/.npmrc）、一个假 npm 全局目录，装的是
// 一个几 KB 的本地包。不会碰你真实的 %USERPROFILE%\.npmrc，也不装任何真实包。
// 不联网：包是本地打的 tarball，不带依赖（npm pack / install 都不需要访问 registry）。
//
// 验证的是这条链路：
//   ① 用户级 ~/.npmrc 里写 prefix=<所选目录>      （DSH Desktop 现在装 DSH 前会做的事）
//   ② npm install -g <tarball>                     （不带 --prefix，模拟终端里的行为）
//   ③ 断言：可执行文件落在**所选目录**里
//   ④ npm uninstall -g <包名>                      （不带任何环境变量 = 用户事后那条命令）
//   ⑤ 断言：包目录与可执行文件都消失了
//   ⑥ 反证：去掉 prefix 行后 npm 又回到默认全局目录（= 当初卸载失败的原因）
//
// 用法：node scripts/verify-npm-uninstall.js
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const npmCmd = path.join(path.dirname(process.execPath), process.platform === 'win32' ? 'npm.cmd' : 'npm');
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'dsh-uninstall-check-'));
const home = path.join(root, 'home');
const globalDir = path.join(root, 'chosen-global-dir');
const pkgSrc = path.join(root, 'pkg');
const pkgName = 'dsh-uninstall-probe';
const binName = pkgName;

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

try {
  fs.mkdirSync(home, { recursive: true });
  fs.mkdirSync(globalDir, { recursive: true });

  const npmrc = path.join(home, '.npmrc');
  const comment = '# 用户的注释行，必须原样保留';
  fs.writeFileSync(npmrc, `${comment}\n`, 'utf8');

  // 造一个最小的本地包（与 DSH 同形：一个 bin 脚本 + package.json），再打成本地 tarball
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

  // ---- ① DSH Desktop 现在会做的事：把所选目录写进用户级 ~/.npmrc 的 prefix= 一行 ----
  fs.writeFileSync(npmrc, `${fs.readFileSync(npmrc, 'utf8')}prefix=${globalDir}\n`, 'utf8');

  const reported = lastLine(run(['config', 'get', 'prefix']).out).trim();
  check(
    'npm 自己解析出的全局目录 = 所选目录',
    reported.toLowerCase() === globalDir.toLowerCase(),
    reported
  );

  // ---- ② 不带 --prefix 的安装（= 终端里的行为） ----
  const install = run(['install', '-g', '--no-audit', '--no-fund', tarball]);
  check('npm install -g <包> 退出码为 0', install.code === 0, lastLine(install.out));

  const pkgDir = path.join(globalDir, 'node_modules', pkgName);
  const shims = ['', '.cmd', '.ps1', '.exe'].map((ext) => path.join(globalDir, binName + ext));
  const anyShim = () => shims.some((p) => fs.existsSync(p));
  check('包目录落在所选目录里', fs.existsSync(pkgDir), pkgDir);
  check('启动脚本落在所选目录里', anyShim(), shims[process.platform === 'win32' ? 1 : 0]);

  // ---- ③ 用**最裸**的方式卸载：不带任何参数、不带任何环境变量覆盖 ----
  const uninstall = run(['uninstall', '-g', pkgName]);
  check('npm uninstall -g <包> 退出码为 0', uninstall.code === 0, lastLine(uninstall.out));

  // ---- ④ 断言卸干净：包目录与启动脚本都不该再存在 ----
  check('包目录已删除', !fs.existsSync(pkgDir), pkgDir);
  check('启动脚本已删除', !anyShim(), shims[1]);
  check('用户的注释行仍在（最小行编辑没破坏别人的配置）', fs.readFileSync(npmrc, 'utf8').includes(comment));

  // ---- ⑤ 反证：没有 prefix= 那一行时，npm 去的是别处 ----
  // 这正是用户当初遇到的现场（DSH 装在所选目录，npm 却去默认目录里找）。
  fs.writeFileSync(npmrc, `${comment}\n`, 'utf8');
  const reportedAfter = lastLine(run(['config', 'get', 'prefix']).out).trim();
  check(
    '去掉 prefix 行后，npm 又回到默认全局目录（= 当初卸载失败的原因）',
    reportedAfter.toLowerCase() !== globalDir.toLowerCase(),
    reportedAfter
  );

  console.log(`\n${failures === 0 ? '全部通过' : `${failures} 项失败`}`);
  process.exitCode = failures === 0 ? 0 : 1;
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
