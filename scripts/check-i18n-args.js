// 静态检查：Rust 里所有 i18n::fmt / i18n::t 调用的**参数表**，元素必须都是 `&…` 形态。
//
// 为什么需要它：参数表的元素类型会被统一成 `&dyn std::fmt::Display`，而裸的 `x`
// （哪怕 `x: &str`）不能直接变成 trait object（`str` 是 unsized），编译报
//   error[E0277]: the size for values of type `str` cannot be known at compilation time
// 这个错误在本项目里已经犯过三次（`&[&eff, d, &pkg]`、`&[dir, pkg]` ×2），
// 而它只有 CI 的 cargo 才会发现 —— 开发机没有 Rust 工具链时，这是唯一的本地护栏。
//
// 用法（在仓库根目录或本目录下都行）：
//   node scripts/check-i18n-args.js                    # 自动扫描 ../src-tauri/src/*.rs
//   node scripts/check-i18n-args.js path/to/a.rs ...    # 只扫指定文件
//
// 退出码：0 = 通过；1 = 发现问题或自测失败（可直接接进任何钩子）。
// 它**先跑自测**：故意写错的样本必须被报出来才继续 —— 否则「没有输出」等于假绿，
// 第一版就是这么骗过作者的（正则写歪了，什么都没扫到却报 OK）。
const fs = require('node:fs');
const path = require('node:path');

/** 把注释与字符串/字符字面量替换成空白（保留长度与换行），便于按括号配对扫描 */
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

function lineOf(src, idx) {
  return src.slice(0, idx).split('\n').length;
}

/** 找出所有裸参数，返回 [{ line, arg }] */
function findBareArgs(raw) {
  const src = blankOut(raw);
  const found = [];
  // 只认 `i18n::fmt(` / `i18n::t(` 的**开头**；键名之后允许出现「键名本身、逗号、空白」，
  // 出现别的字符就说明这不是我们要找的调用。这样既能容忍键名里带括号，又不会像
  // `i18n::t\([^)]*` 那样把 `…as_str()` 的尾巴误当成 `i18n::t(`（第一版就是这么误报的）。
  const re = /i18n::(?:fmt|t)\(/g;
  let m;
  while ((m = re.exec(src)) !== null) {
    let i = m.index + m[0].length;
    let ok = true;
    while (i < src.length) {
      const c = src[i];
      if (c === '&' && src[i + 1] === '[') break;
      if (c === ',' || c === '_' || c === ':' || /\s/.test(c) || /[A-Za-z0-9]/.test(c)) { i += 1; continue; }
      ok = false;
      break;
    }
    if (!ok || i >= src.length) continue; // 这个调用不传参数表（或不是我们的调用）
    const start = i + 1; // 指向 '['
    let depth = 0;
    let j = start;
    for (; j < src.length; j += 1) {
      if (src[j] === '[' || src[j] === '(' || src[j] === '{') depth += 1;
      else if (src[j] === ']' || src[j] === ')' || src[j] === '}') {
        depth -= 1;
        if (depth === 0) break;
      }
    }
    // 按顶层逗号切分参数表
    const body = src.slice(start + 1, j);
    const parts = [];
    let cur = '';
    let d = 0;
    for (const ch of body) {
      if ('([{'.includes(ch)) d += 1;
      else if (')]}'.includes(ch)) d -= 1;
      if (ch === ',' && d === 0) { parts.push(cur); cur = ''; continue; }
      cur += ch;
    }
    if (cur.trim() !== '') parts.push(cur);
    for (const p of parts) {
      const t = p.trim();
      if (t === '' || t.startsWith('&')) continue;
      found.push({ line: lineOf(raw, m.index), arg: t });
    }
  }
  return found;
}

// ---- 自测：故意写错的必须被报出来，正确写法必须被放过 ----
const SELF_TEST = `
fn sample(dir: &str, pkg: &str, eff: String) {
    let _ = i18n::fmt("a", &[dir, pkg]);         // 期望：报 dir、pkg
    let _ = i18n::fmt("b", &[&eff, dir, &pkg]);  // 期望：报 dir
    let _ = i18n::fmt("c", &[&dir, &pkg]);       // 正确：不该报
    let _ = i18n::t("d");                        // 无参数表：不该报
}
`;
const selfFound = findBareArgs(SELF_TEST);
if (selfFound.length !== 3) {
  console.log(`FAIL 扫描器自测：期望 3 处裸参数，实际 ${selfFound.length} 处`);
  for (const h of selfFound) console.log(`     L${h.line}: ${h.arg}`);
  process.exit(1);
}
console.log('OK   扫描器自测通过（已知错误样本被正确报出，不是假绿）');

// ---- 目标文件：给了参数就扫参数，否则自动扫 src-tauri/src/*.rs ----
let files = process.argv.slice(2);
if (files.length === 0) {
  const dir = path.join(__dirname, '..', 'src-tauri', 'src');
  files = fs.existsSync(dir)
    ? fs.readdirSync(dir).filter((f) => f.endsWith('.rs')).map((f) => path.join(dir, f))
    : [];
}
if (files.length === 0) {
  console.log('FAIL 没找到要扫描的 .rs 文件（用法：node scripts/check-i18n-args.js [files...]）');
  process.exit(1);
}

let total = 0;
for (const file of files) {
  const raw = fs.readFileSync(file, 'utf8');
  for (const hit of findBareArgs(raw)) {
    total += 1;
    console.log(`${file}:${hit.line}  裸参数 -> ${hit.arg}`);
  }
}
console.log(
  total === 0
    ? `OK   ${files.length} 个文件里没有裸参数（i18n 参数都是 &&… 形态）`
    : `发现 ${total} 处裸参数 —— 编译会报 E0277（the size for values of type \`str\`），请补上第二层 &`
);
process.exit(total === 0 ? 0 : 1);
