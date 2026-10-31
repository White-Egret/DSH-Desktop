// 静态检查：本项目的版本号在 5 个地方各写了一份，必须完全一致。
//
// 为什么需要它（真实事故，2026-10-31）：把版本升到 1.6.1 时，package.json /
// Cargo.toml / tauri.conf.json / Cargo.lock 都改了，**漏了 package-lock.json** ——
// 它停在 1.5.5，落后两个版本而没人发现。这类不一致不会让任何东西立刻报错：
//   - `npm ci` 用 lock 里的版本号装包，与 package.json 不符时会警告甚至失败；
//   - Tauri 打包用 tauri.conf.json 的 version，与 Cargo.toml 不一致会让
//     「程序自己报告的版本」和「安装包文件名里的版本」对不上；
//   - Cargo.lock 里的 dsh-launcher 版本是 cargo 自动写的，手改 manifest 后
//     不跑一次 cargo 就一直是旧值。
// 人眼核对 5 个文件 6 个字段，漏一个是常态；交给脚本，一秒出结果。
//
// 用法（仓库根目录或本目录下都行）：
//   node scripts/check-versions.js
//
// 退出码：0 = 全部一致；1 = 有不一致或读不出来（可直接接进任何钩子）。
// 它**先跑自测**：故意造一份不一致的样本，必须被报出来才继续 —— 否则「没有输出」
// 等于假绿（同 check-i18n-args.js 的教训：扫描器本身写歪时什么都没扫到却报 OK）。
const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.join(__dirname, '..');

/**
 * 5 个位置 6 个字段。
 * `read` 返回该位置当前的版本号字符串；读不出来返回 null（由调用方报错）。
 */
const SOURCES = [
  {
    label: 'package.json',
    file: 'package.json',
    read: (text) => jsonVersion(text),
  },
  {
    label: 'package-lock.json (根)',
    file: 'package-lock.json',
    read: (text) => jsonVersion(text),
  },
  {
    // npm 的 lock 文件在 packages[""] 里还存了一份，与根版本号是两处
    label: 'package-lock.json (packages[""])',
    file: 'package-lock.json',
    read: (text) => {
      try {
        const j = JSON.parse(text);
        return j && j.packages && j.packages[''] ? j.packages[''].version || null : null;
      } catch (_) {
        return null;
      }
    },
  },
  {
    label: 'src-tauri/Cargo.toml',
    file: 'src-tauri/Cargo.toml',
    // 只认 [package] 段里的那一行 version：文件里还有 rust-version 与大量依赖版本
    read: (text) => {
      const pkg = sectionOf(text, 'package');
      if (pkg === null) return null;
      const m = pkg.match(/^\s*version\s*=\s*"([^"]+)"/m);
      return m ? m[1] : null;
    },
  },
  {
    label: 'src-tauri/tauri.conf.json',
    file: 'src-tauri/tauri.conf.json',
    read: (text) => jsonVersion(text),
  },
  {
    // cargo 把 crate 版本写进 lock；手改 manifest 后不跑 cargo 就一直是旧值
    label: 'src-tauri/Cargo.lock (dsh-launcher)',
    file: 'src-tauri/Cargo.lock',
    read: (text) => {
      const m = text.match(
        /\[\[package\]\]\r?\nname = "dsh-launcher"\r?\nversion = "([^"]+)"/,
      );
      return m ? m[1] : null;
    },
  },
];

/** JSON 文件的顶层 version 字段 */
function jsonVersion(text) {
  try {
    const j = JSON.parse(text);
    return j && typeof j.version === 'string' ? j.version : null;
  } catch (_) {
    return null;
  }
}

/**
 * 取出 TOML 里某个 `[section]` 段落的文本（到下一个 `[` 开头的行为止）。
 * 够用即可：本项目只需要 [package] 这一段，不实现完整 TOML 解析。
 */
function sectionOf(text, name) {
  const lines = text.split(/\r?\n/);
  let start = -1;
  for (let i = 0; i < lines.length; i += 1) {
    if (lines[i].trim() === `[${name}]`) {
      start = i;
      break;
    }
  }
  if (start < 0) return null;
  const body = [];
  for (let i = start + 1; i < lines.length; i += 1) {
    if (/^\s*\[/.test(lines[i])) break;
    body.push(lines[i]);
  }
  return body.join('\n');
}

/**
 * 核心比对：吃一个「读文件」的函数，返回 { ok, rows, error }。
 * 拆出来是为了能自测（喂假的 reader，不碰真实文件）。
 */
function compare(readFile) {
  const rows = [];
  for (const src of SOURCES) {
    let text;
    try {
      text = readFile(src.file);
    } catch (_) {
      rows.push({ label: src.label, version: null, error: '文件读不出来' });
      continue;
    }
    const version = src.read(text);
    rows.push({
      label: src.label,
      version,
      error: version ? null : '解析不出 version',
    });
  }
  const bad = rows.filter((r) => r.error);
  if (bad.length) {
    return { ok: false, rows, error: `${bad.length} 个位置解析失败` };
  }
  const values = new Set(rows.map((r) => r.version));
  if (values.size > 1) {
    return { ok: false, rows, error: `${values.size} 个不同的版本号` };
  }
  return { ok: true, rows, error: null };
}

// ---- 自测：故意造一份不一致的样本，必须被报出来 ----
(function selfTest() {
  const consistent = {
    'package.json': '{"version":"9.9.9"}',
    'package-lock.json': '{"version":"9.9.9","packages":{"":{"version":"9.9.9"}}}',
    'src-tauri/Cargo.toml': '[package]\nname = "x"\nversion = "9.9.9"\n\n[dependencies]\nfoo = { version = "1.0" }\n',
    'src-tauri/tauri.conf.json': '{"version":"9.9.9"}',
    'src-tauri/Cargo.lock': '[[package]]\nname = "dsh-launcher"\nversion = "9.9.9"\n',
  };
  const fake = (files) => (f) => {
    if (!(f in files)) throw new Error('missing');
    return files[f];
  };

  // ① 全部一致 → 必须通过
  const pass = compare(fake(consistent));
  if (!pass.ok) {
    console.log(`FAIL 自测：一致样本被误判为不一致（${pass.error}）`);
    process.exit(1);
  }

  // ② 只改 package-lock 的根版本（复刻 1.5.5 落后两个版本那次）→ 必须报错
  const drift = { ...consistent, 'package-lock.json': '{"version":"9.9.8","packages":{"":{"version":"9.9.9"}}}' };
  if (compare(fake(drift)).ok) {
    console.log('FAIL 自测：package-lock 版本漂移没被发现');
    process.exit(1);
  }

  // ③ Cargo.lock 里的 crate 版本漂移（手改 manifest 后没跑 cargo）→ 必须报错
  const lockDrift = {
    ...consistent,
    'src-tauri/Cargo.lock': '[[package]]\nname = "dsh-launcher"\nversion = "9.9.8"\n',
  };
  if (compare(fake(lockDrift)).ok) {
    console.log('FAIL 自测：Cargo.lock 版本漂移没被发现');
    process.exit(1);
  }

  // ④ 文件缺失 → 必须报错（而不是当成「没有版本号」静默通过）
  const missing = { ...consistent };
  delete missing['src-tauri/tauri.conf.json'];
  if (compare(fake(missing)).ok) {
    console.log('FAIL 自测：缺失文件没被发现');
    process.exit(1);
  }

  console.log('OK   扫描器自测通过（四种偏差样本都被正确报出，不是假绿）');
})();

// ---- 真实比对 ----
const result = compare((f) => fs.readFileSync(path.join(ROOT, f), 'utf8'));
for (const row of result.rows) {
  const shown = row.error ? `  ✘ ${row.error}` : row.version;
  console.log(`  ${row.label.padEnd(38)} ${shown}`);
}

if (result.ok) {
  console.log(`OK   6 个位置的版本号一致：${result.rows[0].version}`);
  process.exit(0);
}

console.log(`\nFAIL ${result.error}。把上面列出的位置改成同一个版本号（注意 package-lock.json 有 2 处）。`);
process.exit(1);
