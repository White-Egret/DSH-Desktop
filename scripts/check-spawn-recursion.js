// 调用链自检：找出「某个函数通过『启动子进程』这条路绕回自己」的环。
//
// 为什么需要它 —— 2026-10-06 真机崩溃（`0xc00000fd` 栈溢出，双击图标没有任何窗口）：
//
//   npm_global_bin_dirs() → npm_prefix_for_global_bins() → find_npm_cmd()
//     → 启动 npm（run_capture_timeout_in）→ child_path_for()   ← 「启动任何子进程」都走它
//       → npm_global_bin_dirs()                                ← 回到起点，无限递归
//
// 这类环有两个特点，正是它值得一个专用检查器的原因：
//   ① **编译器不会报错** —— 它能编译、能过单测，只在真机上把栈打穿；
//   ② **开发机没有 Rust 工具链时几乎无法发现** —— 只能等用户双击图标、什么也不发生。
//
// 做法：把源码按顶层 `fn` 粗切，标出「含 spawn 痕迹」的函数（Command::new / .output() /
// run_capture_timeout* / where_all / registry_path_raw 等），再做可达性搜索：
// 若从 A 出发、**经过至少一条 spawn 边**能回到 A，就报出来。
//
// 用法（在仓库根目录或本目录下都行）：
//   node scripts/check-spawn-recursion.js
//   node scripts/check-spawn-recursion.js path/to/src-tauri/src
//
// 退出码：0 = 没有环；1 = 有可疑环（或没找到源码目录）。
// 注意：这是**粗粒度文本分析**，会有假阳性（同名函数、宏展开、动态调用都看不见）。
// 它的定位是「提交前扫一眼」，不是替代编译器；报出来的环要人工确认一遍。
const fs = require('node:fs');
const path = require('node:path');

const srcDir = process.argv[2] || path.join(__dirname, '..', 'src-tauri', 'src');
if (!fs.existsSync(srcDir)) {
  console.log(`FAIL 找不到源码目录：${srcDir}`);
  process.exit(1);
}
const files = fs.readdirSync(srcDir).filter((f) => f.endsWith('.rs'));

// 「会启动子进程」的痕迹。命中任一即认为该函数带 spawn 边。
const SPAWN_MARKERS = [
  'Command::new',
  '.output()',
  '.spawn()',
  'where_all(',
  'where_lookup(',
  'run_capture_timeout',
  'run_cmd_capture',
  'run_taskkill',
  'registry_path_raw(', // 内部起 reg.exe
];

/** 极简函数切分：顶层 `fn name(` 到对应的收尾 `}` 当正文（够这个仓库用） */
function splitFunctions(src) {
  const lines = src.split(/\r?\n/);
  const fns = [];
  let cur = null;
  let depth = 0;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const m = /^\s*(?:pub(?:\(crate\))?\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)/.exec(line);
    if (m && depth === 0) {
      if (cur) fns.push(cur);
      cur = { name: m[1], start: i + 1, body: [] };
    }
    if (cur) cur.body.push(line);
    for (const ch of line) {
      if (ch === '{') depth += 1;
      else if (ch === '}') depth -= 1;
    }
    if (depth === 0 && cur && /^\}/.test(line)) {
      fns.push(cur);
      cur = null;
    }
  }
  if (cur) fns.push(cur);
  return fns;
}

const allFns = [];
for (const f of files) {
  const src = fs.readFileSync(path.join(srcDir, f), 'utf8');
  for (const fn of splitFunctions(src)) allFns.push({ ...fn, file: f });
}
const byName = new Map();
for (const fn of allFns) if (!byName.has(fn.name)) byName.set(fn.name, fn);

/** 某函数体内调用了哪些本仓函数 */
function callsOf(fn) {
  const body = fn.body.join('\n');
  const calls = new Set();
  for (const other of byName.keys()) {
    if (other === fn.name) continue;
    if (new RegExp(`\\b${other}\\s*\\(`).test(body)) calls.add(other);
  }
  return calls;
}
/** 某函数是否含 spawn 边 */
function spawns(fn) {
  const body = fn.body.join('\n');
  return SPAWN_MARKERS.some((mk) => body.includes(mk));
}

// 从每个函数出发，找「经过至少一条 spawn 边后能回到自己」的路径
const problems = [];
for (const start of byName.values()) {
  const seen = new Set();
  const queue = [[start.name, false, [start.name]]]; // (当前函数, 是否已过 spawn 边, 路径)
  while (queue.length) {
    const [name, usedSpawn, trail] = queue.shift();
    const key = `${name}|${usedSpawn}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const fn = byName.get(name);
    if (!fn) continue;
    const nextSpawn = usedSpawn || spawns(fn);
    for (const callee of callsOf(fn)) {
      const trail2 = [...trail, callee];
      if (callee === start.name && nextSpawn) {
        problems.push(trail2);
        continue;
      }
      if (trail2.length > 12) continue; // 更长的环在起点处已经报过
      queue.push([callee, nextSpawn, trail2]);
    }
  }
}

const uniq = new Map();
for (const p of problems) if (!uniq.has(p.join(' -> '))) uniq.set(p.join(' -> '), p);

if (uniq.size === 0) {
  console.log(`OK   没有「经过子进程启动绕回自己」的调用环（扫描 ${byName.size} 个函数）`);
  process.exit(0);
}
console.log(`发现 ${uniq.size} 条可疑环（真机上会表现为栈溢出 0xc00000fd、程序起不来）：`);
for (const p of uniq.values()) console.log('  ' + p.join(' -> '));
process.exit(1);
