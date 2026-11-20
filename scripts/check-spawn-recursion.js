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
// ── 2026-11-20 的两类假阳性修复（真机事件：一次跑分刷出 186 条「环」，全是假的）──
//
//   ① **字符串/注释里的英文文案被当成调用**。`i18n.rs` 的文案 "…did not run
//      (--version failed)…" 里含 `run (`，被认成「t 调用了 lib::run」；这条假边
//      与真实图里 `run → … → t` 的合法路径一拼，每个真环都能被它闭合 —— 于是
//      186 条环条条穿过 `fmt -> t -> run -> start_dsh`。修法：匹配前先跑
//      blankOut（照抄 check-i18n-args.js，保留长度与换行），注释与字符串全部
//      变空白，只剩代码参与匹配。
//   ② **方法调用被当成自由函数调用**。`b.run()` 是某个 builder 的方法，不是
//      `fn run`；`\b` 的边界在 `.` 与 `r` 之间照样成立。修法：调用边正则改用
//      `(?<![.\w])name\s*\(` —— 前面是 `.` 的一律不算（`.spawn()` 这类仍由
//      SPAWN_MARKERS 单独判，不受影响）。**`::name(` 保留**：`crate::process::
//      run_cmd_capture(` 是货真价实的跨模块调用，排除它就是把检测能力削没，
//      自测里专门有一条（D）钉住这个边界。
//
// ── 自测（防假绿，check-i18n-args.js 同一条教训）──
//
// 扫描器写歪时「什么都没扫到却报 OK」是最坏结局，所以主扫描前先用五个合成样本
// 过双向判据：真环必须抓到、假阳性陷阱必须不抓。而且**陷阱样本还要在「旧规则」
// （不 blankOut、用 \b）下确实能触发假阳性** —— 证明陷阱是活的；否则谁把样本
// 改成一段无害代码，自测就成了新的假绿。
//
// 用法（在仓库根目录或本目录下都行）：
//   node scripts/check-spawn-recursion.js
//   node scripts/check-spawn-recursion.js path/to/src-tauri/src
//
// 退出码：0 = 没有环；1 = 有可疑环、自测失败、或没找到源码目录。
// 注意：这仍是**粗粒度文本分析**（同名函数先到先得、宏展开与动态调用看不见），
// 定位是「提交前扫一眼」，不是替代编译器；报出来的环要人工确认一遍。
const fs = require('node:fs');
const path = require('node:path');

/**
 * 把注释与字符串/字符字面量替换成空白（保留长度与换行）。
 * 与 check-i18n-args.js 的同名函数**逐字一致** —— 那边已经在本仓库 17 个 .rs 上
 * 验证过（嵌套块注释、转义、生命周期 'a 不误伤），这里不另立一份变体。
 */
function blankOut(src) {
  const out = src.split('');
  const n = src.length;
  let i = 0;
  let blockDepth = 0;
  while (i < n) {
    const c = src[i];
    const c2 = src[i + 1];
    if (blockDepth > 0) {
      if (c === '/' && c2 === '*') { blockDepth += 1; out[i] = ' '; out[i + 1] = ' '; i += 2; continue; }
      if (c === '*' && c2 === '/') { blockDepth -= 1; out[i] = ' '; out[i + 1] = ' '; i += 2; continue; }
      if (c !== '\n') out[i] = ' ';
      i += 1; continue;
    }
    if (c === '/' && c2 === '/') {
      while (i < n && src[i] !== '\n') { out[i] = ' '; i += 1; }
      continue;
    }
    if (c === '/' && c2 === '*') { blockDepth = 1; out[i] = ' '; out[i + 1] = ' '; i += 2; continue; }
    if (c === '"') {
      out[i] = ' '; i += 1;
      while (i < n) {
        if (src[i] === '\\') { out[i] = ' '; if (i + 1 < n) out[i + 1] = ' '; i += 2; continue; }
        if (src[i] === '"') { out[i] = ' '; i += 1; break; }
        if (src[i] !== '\n') out[i] = ' ';
        i += 1;
      }
      continue;
    }
    if (c === "'") {
      if (src[i + 1] === '\\') {
        // 转义字符字面量 '\n'
        out[i] = ' '; i += 1;
        while (i < n && src[i] !== "'") { out[i] = ' '; i += 1; }
        if (i < n) { out[i] = ' '; i += 1; }
        continue;
      }
      if (src[i + 2] === "'") {
        // 普通字符字面量 'x'
        out[i] = ' '; out[i + 1] = ' '; out[i + 2] = ' '; i += 3; continue;
      }
      i += 1; continue; // 生命周期 'a：原样保留
    }
    i += 1;
  }
  return out.join('');
}

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

/** 极简函数切分：顶层 `fn name(` 到对应的收尾 `}` 当正文（够这个仓库用）。
 *  传进来的应当是 blankOut 之后的文本 —— 字符串/注释里的花括号就不再干扰深度了。 */
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

/**
 * 核心分析。sources = [{file, src}]。
 *
 * opts.legacy = true 用**修复前**的规则（不 blankOut、`\b` 匹配）—— 只给自测的
 * 「陷阱活性校验」用，主扫描永远走新规则。
 */
function analyze(sources, opts = {}) {
  const legacy = opts.legacy === true;
  const allFns = [];
  for (const { file, src } of sources) {
    for (const fn of splitFunctions(legacy ? src : blankOut(src))) allFns.push({ ...fn, file });
  }
  const byName = new Map();
  for (const fn of allFns) if (!byName.has(fn.name)) byName.set(fn.name, fn);

  // 调用边匹配：新规则排除「前面是 .」的方法调用；`::`（限定路径）与行首/空白
  // 照常算。legacy 规则复刻当年的 `\b` —— 它在 `.` 与字母之间同样成立，这正是
  // 假阳性 ② 的来源。
  const reCache = new Map();
  const calleeRe = (name) => {
    if (!reCache.has(name)) {
      reCache.set(name, legacy
        ? new RegExp(`\\b${name}\\s*\\(`)
        : new RegExp(`(?<![.\\w])${name}\\s*\\(`));
    }
    return reCache.get(name);
  };

  const callCache = new Map();
  /** 某函数体内调用了哪些本仓函数 */
  function callsOf(fn) {
    if (callCache.has(fn)) return callCache.get(fn);
    const body = fn.body.join('\n');
    const calls = new Set();
    for (const other of byName.keys()) {
      if (other === fn.name) continue;
      if (calleeRe(other).test(body)) calls.add(other);
    }
    callCache.set(fn, calls);
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
  return { problems: [...uniq.values()], fnCount: byName.size };
}

// ---------- 自测（主扫描之前跑；任何一条不符就拒绝给出「OK」） ----------

const CASES = [
  {
    name: 'A 真环（2026-10-06 历史形状：spawn 之后绕回起点）',
    // 新规则必须抓到；旧规则同样能抓（这条不涉假阳性，只钉「检测能力还在」）。
    expectCycle: true, legacyExpectCycle: true,
    sources: [{ file: 'a.rs', src: [
      'fn npm_global_bin_dirs() {',
      '    child_path_for()',
      '}',
      'fn child_path_for() {',
      '    Command::new("x");',
      '    npm_global_bin_dirs()',
      '}',
    ].join('\n') }],
  },
  {
    name: 'B 字符串文案陷阱（英文文案里的 "run ("）',
    // 真机 186 条假环的元凶形状（i18n.rs:760）。新规则必须**不**报；
    // 旧规则必须**能**报 —— 证明这个陷阱样本是活的。
    expectCycle: false, legacyExpectCycle: true,
    sources: [{ file: 'b.rs', src: [
      'fn t() {',
      '    "The freshly unpacked {0} did not run (--version failed); discarded."',
      '}',
      'fn run() {',
      '    Command::new("x");',
      '    t()',
      '}',
    ].join('\n') }],
  },
  {
    name: 'C 方法调用陷阱（b.run() 不是 fn run）',
    expectCycle: false, legacyExpectCycle: true,
    sources: [{ file: 'c.rs', src: [
      'fn outer() {',
      '    let b = Builder::default();',
      '    b.run();',
      '}',
      'fn run() {',
      '    Command::new("x");',
      '    outer()',
      '}',
    ].join('\n') }],
  },
  {
    name: 'D 限定路径 crate::run( 是真调用，必须仍算边（防排除过头）',
    expectCycle: true, legacyExpectCycle: true,
    sources: [{ file: 'd.rs', src: [
      'fn a() {',
      '    crate::run()',
      '}',
      'fn run() {',
      '    Command::new("x");',
      '    a()',
      '}',
    ].join('\n') }],
  },
  {
    name: 'E 注释陷阱（// 文案里的 "run ("）',
    expectCycle: false, legacyExpectCycle: true,
    sources: [{ file: 'e.rs', src: [
      'fn t() {',
      '    // the installer did not run (never showed up)',
      '}',
      'fn run() {',
      '    Command::new("x");',
      '    t()',
      '}',
    ].join('\n') }],
  },
];

function selfTest() {
  let fail = 0;
  for (const c of CASES) {
    const now = analyze(c.sources).problems.length > 0;
    const was = analyze(c.sources, { legacy: true }).problems.length > 0;
    if (now !== c.expectCycle) {
      console.log(`FAIL 自测 ${c.name}：现行规则${now ? '抓到' : '没抓到'}环，期望${c.expectCycle ? '抓到' : '不报'}`);
      fail += 1;
    }
    if (was !== c.legacyExpectCycle) {
      console.log(`FAIL 自测 ${c.name}：陷阱活性校验失败 —— 旧规则${was ? '抓到' : '没抓到'}，` +
        `期望${c.legacyExpectCycle ? '抓到（样本必须真含当年那种假阳性，否则自测本身是假绿）' : '不报'}`);
      fail += 1;
    }
  }
  if (fail > 0) {
    console.log(`自测 ${fail} 项未通过 —— 扫描器本身不可信，主扫描结果不采信。`);
    process.exit(1);
  }
  console.log(`OK   扫描器自测通过（${CASES.length} 组样本：真环抓到、假阳性陷阱不报、` +
    '且每个陷阱在旧规则下确实会误报 —— 样本是活的）');
}

// ---------- 主扫描 ----------

selfTest();

const srcDir = process.argv[2] || path.join(__dirname, '..', 'src-tauri', 'src');
if (!fs.existsSync(srcDir)) {
  console.log(`FAIL 找不到源码目录：${srcDir}`);
  process.exit(1);
}
const sources = fs.readdirSync(srcDir)
  .filter((f) => f.endsWith('.rs'))
  .map((f) => ({ file: f, src: fs.readFileSync(path.join(srcDir, f), 'utf8') }));

const { problems, fnCount } = analyze(sources);
if (problems.length === 0) {
  console.log(`OK   没有「经过子进程启动绕回自己」的调用环（扫描 ${fnCount} 个函数）`);
  process.exit(0);
}
console.log(`发现 ${problems.length} 条可疑环（真机上会表现为栈溢出 0xc00000fd、程序起不来）：`);
for (const p of problems) console.log('  ' + p.join(' -> '));
process.exit(1);
