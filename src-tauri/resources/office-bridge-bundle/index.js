// DSH Desktop —— OnlyOffice 能力桥接：Host 半身（P1）
//
// 职责：
//   ① 起一个只绑 127.0.0.1 的文档服务：
//        GET  /file/<token>      Document Server 回源取文件（服务端，无 CORS 概念）
//        GET  /editor/<token>    编辑器宿主页（加载 api.js，config 内嵌 JWT）
//        POST /callback/<docId>  保存回调（status 2/6 → 下载新版本 → 写回原文件）
//        GET  /docs              已注册文档列表（供右侧栏 UI，P2 用）
//        POST /upload?name=…     上传到文档目录（供右侧栏 UI，P2 用）
//        GET  /status            健康摘要
//        POST /browse?path=…     用系统默认浏览器打开该文档（右侧栏按钮；内嵌 webview 里 window.open 开不出窗口）
//   ② 注册六个 AI 工具：office_status / office_open / office_convert /
//      office_read_text / office_forcesave / office_generate
//
// ⚠ 零顶层运行时 import（只 `node:` 内置）——这是 py-bridge-bundle 用真机故障换来的
//   架构约束：profile 的 node_modules 里没有 `@deepseek-ai/*`，顶层 import 裸包名会让
//   整个插件在模块加载期 ERR_MODULE_NOT_FOUND，apply() 从未被调用、一个工具都不注册，
//   而日志里只有一句 failed to import。`node:` 前缀的内置模块不受此限。
//
// ⚠ 手写 tool 对象直接 ctx.tools.register，不走 defineTool（同上：不可用）。
//   output.schema 用 {}（dsh-tools 只接受 JSON Schema 子集，且空 schema 是官方认可的
//   「无约束 JSON」形式）。每个工具单独 try/catch，一个坏了不连累其余五个。
//
// ⚠ 桥接没配好（无 jwtSecret / Document Server 不可达）时**安静降级**：DSH 必须照常
//   可用，插件不是 DSH 的启动依赖。

import { spawn } from 'node:child_process'
import { createHash, createHmac, randomBytes, timingSafeEqual } from 'node:crypto'
import {
  createReadStream, createWriteStream, existsSync, mkdirSync,
  readFileSync, readdirSync, statSync, unlinkSync, writeFileSync,
} from 'node:fs'
import http from 'node:http'
import https from 'node:https'
import { createServer } from 'node:http'
import path from 'node:path'

const LOG_PREFIX = '[office-bridge]'
const DEFAULT_DOCSERVER = 'http://127.0.0.1:8080'
const DEFAULT_PORT = 3310
/** 文件/编辑器链接有效期：12 小时，够一次工作会话。 */
const TOKEN_TTL_MS = 12 * 60 * 60 * 1000
/** office_read_text 的正文上限：超过就落盘只回路径+预览（与 py-bridge 同一条纪律）。 */
const MAX_TEXT_CHARS = 20000
/** 上传体积上限：100 MB。 */
const MAX_UPLOAD_BYTES = 100 * 1024 * 1024
/** 认得出编辑器的扩展名 → Document Server 的 documentType。 */
const DOC_TYPES = {
  docx: 'word', doc: 'word', odt: 'word', rtf: 'word', txt: 'word', epub: 'word',
  xlsx: 'cell', xls: 'cell', ods: 'cell', csv: 'cell',
  pptx: 'slide', ppt: 'slide', odp: 'slide',
  // PDF 是 Document Server 10.x 的**一等编辑器**（web-apps/apps/pdfeditor，
  // documentType = 'pdf'）。真机教训 2026-11-05：缺这一条时，上传能进目录、
  // 列表里也看得见，但「编辑」「浏览器」两个按钮都会被扩展名校验拒掉 ——
  // 用户看到的是「文件在列表里，却打不开」。
  pdf: 'pdf',
}
const OFFICE_EXTS = Object.keys(DOC_TYPES)

// ---------- 纯函数 ----------

const b64url = (buf) => Buffer.from(buf).toString('base64url')

/** 编辑器宿主页里那种「密文令牌」：payload + HMAC，用途是让 URL 自带鉴权。 */
function issueToken(secret, payload, ttlMs = TOKEN_TTL_MS) {
  const raw = b64url(JSON.stringify({ ...payload, e: Date.now() + ttlMs }))
  const sig = createHmac('sha256', secret).update(raw).digest('base64url')
  return `${raw}.${sig}`
}

/** 校验令牌；过期、签名不符、结构损坏一律 null（调用方回 403，不区分原因）。 */
function verifyToken(secret, token) {
  if (typeof token !== 'string') return null
  const i = token.lastIndexOf('.')
  if (i <= 0) return null
  const raw = token.slice(0, i)
  const sig = token.slice(i + 1)
  const want = createHmac('sha256', secret).update(raw).digest('base64url')
  if (sig.length !== want.length || !timingSafeEqual(Buffer.from(sig), Buffer.from(want))) return null
  try {
    const obj = JSON.parse(Buffer.from(raw, 'base64url').toString('utf8'))
    if (!obj || typeof obj.e !== 'number' || obj.e < Date.now()) return null
    return obj
  } catch {
    return null
  }
}

/** Document Server 的 JWT：header.payload.HS256。 */
function signJwt(secret, payload) {
  const h = b64url(JSON.stringify({ alg: 'HS256', typ: 'JWT' }))
  const p = b64url(JSON.stringify(payload))
  const sig = createHmac('sha256', secret).update(`${h}.${p}`).digest('base64url')
  return `${h}.${p}.${sig}`
}

/** 后缀 → documentType；不认识的按 word 处理（DS 自己会拒）。 */
function docTypeFor(ext) {
  return DOC_TYPES[String(ext || '').toLowerCase()] || 'word'
}

/**
 * document.key：同一次「打开」在服务端是同一个编辑器缓存，保存后必须换新值，
 * 否则重开会命中旧缓存、表现为「版本已变化」或内容不更新。
 * 取 path + mtime + size + 打开计数 的哈希：文件没变且没保存 → key 稳定（恢复会话）；
 * 保存回调后计数 +1 → 下次打开是新 key。
 */
function computeDocKey(absPath, opens) {
  const st = statSync(absPath)
  return createHash('sha256')
    .update(`${absPath}|${st.mtimeMs}|${st.size}|${opens}`)
    .digest('hex')
    .slice(0, 20)
}

/** 断言路径是存在的普通文件，返回绝对路径。 */
function requireFile(p) {
  if (typeof p !== 'string' || !p.trim()) throw new Error('path 不能为空')
  const abs = path.resolve(p)
  if (!existsSync(abs)) throw new Error(`文件不存在：${abs}`)
  if (!statSync(abs).isFile()) throw new Error(`不是普通文件：${abs}`)
  return abs
}

function basenameSansExt(absPath) {
  return path.basename(absPath, path.extname(absPath))
}

/** 唯一输出路径：同名已存在时加 -1 / -2 …… */
function uniquePath(dir, name) {
  const ext = path.extname(name)
  const stem = name.slice(0, name.length - ext.length)
  let candidate = path.join(dir, name)
  let i = 1
  while (existsSync(candidate)) {
    candidate = path.join(dir, `${stem}-${i}${ext}`)
    i += 1
  }
  return candidate
}

/** 读请求体（带体积上限；超限直接 destroy 并抛错）。 */
function readBody(req, limit = 8 * 1024 * 1024) {
  return new Promise((resolve, reject) => {
    const chunks = []
    let size = 0
    req.on('data', (c) => {
      size += c.length
      if (size > limit) {
        reject(new Error(`请求体超过上限 ${limit} 字节`))
        req.destroy()
        return
      }
      chunks.push(c)
    })
    req.on('end', () => resolve(Buffer.concat(chunks)))
    req.on('error', reject)
  })
}

/**
 * 用系统默认浏览器打开一个 URL。
 *
 * 为什么需要它：Client 半身跑在 DSH Desktop 的 Tauri 子 webview（label=dsh）里，
 * 那个 webview 被刻意排除在 Tauri 权限之外，`window.open` 创建不出新窗口 —— 点了静默无事发生。
 * 机制与 DSH Desktop 自己的 `process::open_in_browser` 相同：rundll32 url.dll,FileProtocolHandler。
 * 参数经 argv 直传、不经 cmd.exe，URL 里的 & 不会被当成命令分隔符。
 */
function launchBrowser(target) {
  // spawn 的失败（找不到 rundll32、无权限）走的是异步 error 事件，不挂监听就是静默成功。
  // 所以等它 120ms：这段时间内没有 error，就认为已经交给系统了（浏览器由 rundll32 自己异步拉起）。
  return new Promise((resolve) => {
    let settled = false
    const settle = (v) => { if (!settled) { settled = true; resolve(v) } }
    let child
    try {
      child = spawn('rundll32.exe', ['url.dll,FileProtocolHandler', target], {
        detached: true,
        stdio: 'ignore',
        windowsHide: true,
      })
    } catch (e) {
      settle({ ok: false, error: e.message })
      return
    }
    child.once('error', (e) => settle({ ok: false, error: e.message }))
    setTimeout(() => settle({ ok: true }), 120)
    child.unref()
  })
}

/** 跟随重定向下载到本地文件（Document Server 的 fileUrl 可能 302 到 cache）。 */
function downloadTo(url, destPath, hops = 0) {
  return new Promise((resolve, reject) => {
    if (hops > 5) return reject(new Error('重定向过多'))
    let u
    try { u = new URL(url) } catch { return reject(new Error(`非法 URL：${url}`)) }
    const lib = u.protocol === 'https:' ? https : http
    const req = lib.get(u, (res) => {
      const sc = res.statusCode || 0
      if (sc >= 300 && sc < 400 && res.headers.location) {
        res.resume()
        return downloadTo(new URL(res.headers.location, u).href, destPath, hops + 1).then(resolve, reject)
      }
      if (sc !== 200) {
        res.resume()
        return reject(new Error(`下载失败 HTTP ${sc}：${url}`))
      }
      const out = createWriteStream(destPath)
      res.pipe(out)
      out.on('finish', () => resolve(destPath))
      out.on('error', reject)
    })
    req.on('error', reject)
    req.setTimeout(120_000, () => { req.destroy(new Error('下载超时')) })
  })
}

/** POST JSON，返回解析后的 JSON（非 2xx 也尝试解析，交给调用方看 error 字段）。 */
function postJson(url, payload, timeoutMs = 120_000) {
  return new Promise((resolve, reject) => {
    let u
    try { u = new URL(url) } catch { return reject(new Error(`非法 URL：${url}`)) }
    const lib = u.protocol === 'https:' ? https : http
    const body = Buffer.from(JSON.stringify(payload), 'utf8')
    const req = lib.request({
      protocol: u.protocol, hostname: u.hostname, port: u.port, path: u.pathname + u.search,
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Content-Length': body.length, Accept: 'application/json' },
    }, (res) => {
      const chunks = []
      res.on('data', (c) => chunks.push(c))
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8')
        try { resolve({ status: res.statusCode, json: JSON.parse(text) }) } catch {
          reject(new Error(`响应不是 JSON（HTTP ${res.statusCode}）：${text.slice(0, 200)}`))
        }
      })
    })
    req.on('error', reject)
    req.setTimeout(timeoutMs, () => { req.destroy(new Error('请求超时')) })
    req.end(body)
  })
}

/** GET 文本（健康检查用）。 */
function getText(url, timeoutMs = 15_000) {
  return new Promise((resolve, reject) => {
    let u
    try { u = new URL(url) } catch { return reject(new Error(`非法 URL：${url}`)) }
    const lib = u.protocol === 'https:' ? https : http
    const req = lib.get(u, (res) => {
      const chunks = []
      res.on('data', (c) => chunks.push(c))
      res.on('end', () => resolve({ status: res.statusCode, text: Buffer.concat(chunks).toString('utf8') }))
    })
    req.on('error', reject)
    req.setTimeout(timeoutMs, () => { req.destroy(new Error('请求超时')) })
  })
}

/** 去掉转换得到的 txt 可能带的 BOM。 */
function stripBom(s) {
  return s.charCodeAt(0) === 0xfeff ? s.slice(1) : s
}

/**
 * 把生成脚本规范化成「一行一条语句」。
 *
 * DS 10.0.0 的 docbuilder 对脚本的换行极其敏感（实测，2026-11-03）：
 *   多行脚本            → 正常产出 urls
 *   全部挤在一行        → **静默返回空 urls**（既不报错也不产出文件）
 *   一行里多条语句      → error=-3
 * 而 agent 几乎必然写成一行（"builder.CreateFile(...);var d=...;..."）。所以在这里
 * 按「字符串字面量之外的 `;`」切行——JS 里换行是空白，切完语义不变。
 */
function normalizeScript(script) {
  const src = String(script || '').replace(/\r\n?/g, '\n')
  let out = ''
  let quote = null
  let escaped = false
  for (const ch of src) {
    if (escaped) { out += ch; escaped = false; continue }
    if (ch === '\\') { out += ch; escaped = true; continue }
    if (quote) { out += ch; if (ch === quote) quote = null; continue }
    if (ch === '"' || ch === "'" || ch === '`') { out += ch; quote = ch; continue }
    if (ch === ';') { out += ';\n'; continue }
    out += ch
  }
  // 压掉多余空行，但保留语句间的换行
  return out.split('\n').map((l) => l.trimEnd()).filter((l, i, arr) => l.trim() !== '' || (i > 0 && arr[i - 1].trim() !== '')).join('\n')
}

// ---------- 插件主体 ----------

/**
 * @param {import('@deepseek-ai/dsh').Context} ctx
 * @param {{ docserverUrl?: string, jwtSecret?: string, docPort?: number, workspaceDir?: string }} config
 */
export function apply(ctx, config = {}) {
  const docserverUrl = String(config.docserverUrl || DEFAULT_DOCSERVER).replace(/\/+$/, '')
  const secret = String(config.jwtSecret || '').trim()
  const wantedPort = Number(config.docPort) > 0 ? Number(config.docPort) : DEFAULT_PORT
  const docsDir = String(config.workspaceDir || '').trim()
    || path.join(process.env.USERPROFILE || process.env.HOME || '.', 'Documents', 'DSH-Office')
  /** 实际监听端口：配置端口被占时会顺延，所以这里记真值，所有 URL 都用它。 */
  let actualPort = wantedPort
  const baseUrlFor = (port) => `http://127.0.0.1:${port}`

  const log = (level, msg) => {
    const fn = ctx.logger?.[level] || ctx.logger?.info
    try { fn?.(`${LOG_PREFIX} ${msg}`) } catch { /* 日志失败不影响主线 */ }
  }

  // 没配密钥就安静退出：DSH 照常可用（与 py-bridge「没装桥接就什么都不做」同一条纪律）
  if (!secret) {
    log('warn', '未配置 jwtSecret，跳过注册（请在「安装/卸载」里重新部署 OnlyOffice 桥接）')
    return () => {}
  }

  /** @type {Map<string, { id: string, path: string, opens: number, name: string, openedAt: number }>} */
  const docs = new Map()
  let server = null

  // ---- 文档注册（office_open 与 /upload 共用） ----

  function registerDoc(absPath) {
    const id = createHash('sha256').update(absPath).digest('hex').slice(0, 16)
    const prev = docs.get(id)
    const entry = {
      id,
      path: absPath,
      name: path.basename(absPath),
      opens: prev ? prev.opens + 1 : 1,
      openedAt: Date.now(),
    }
    docs.set(id, entry)
    return entry
  }

  /** 给「Document Server 要回源取的文件」发一个短期令牌 URL。 */
  function fetchUrlFor(absPath) {
    const token = issueToken(secret, { k: 'file', p: absPath, n: path.basename(absPath) })
    return `${baseUrlFor(actualPort)}/file/${token}`
  }

  function editorUrlFor(entry) {
    const token = issueToken(secret, { k: 'editor', d: entry.id })
    return `${baseUrlFor(actualPort)}/editor/${token}`
  }

  // ---- 文档服务 ----

  function startServer() {
    if (!existsSync(docsDir)) {
      try { mkdirSync(docsDir, { recursive: true }) } catch (e) {
        log('error', `无法创建文档目录 ${docsDir}：${e.message}`)
        return
      }
    }

    server = createServer((req, res) => {
      const url = new URL(req.url || '/', baseUrlFor(actualPort))
      const p = url.pathname

      // 供浏览器侧（P2 的 Client 半身）跨源调用：只放行本机来源
      const origin = req.headers.origin
      if (typeof origin === 'string' && /^https?:\/\/(127\.0\.0\.1|localhost)(:\d+)?$/.test(origin)) {
        res.setHeader('Access-Control-Allow-Origin', origin)
        res.setHeader('Vary', 'Origin')
        res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS')
        res.setHeader('Access-Control-Allow-Headers', 'Content-Type')
      }
      if (req.method === 'OPTIONS') { res.writeHead(204).end(); return }

      const sendJson = (code, obj) => {
        const body = Buffer.from(JSON.stringify(obj), 'utf8')
        res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': body.length })
        res.end(body)
      }

      try {
        // ---- 取文件（Document Server 回源） ----
        if (p.startsWith('/file/')) {
          const claim = verifyToken(secret, p.slice('/file/'.length))
          if (!claim || claim.k !== 'file') return sendJson(403, { error: 'forbidden' })
          const abs = String(claim.p || '')
          if (!existsSync(abs) || !statSync(abs).isFile()) return sendJson(404, { error: 'not found' })
          const st = statSync(abs)
          res.writeHead(200, {
            'Content-Type': 'application/octet-stream',
            'Content-Length': st.size,
            'Content-Disposition': `attachment; filename*=UTF-8''${encodeURIComponent(claim.n || path.basename(abs))}`,
          })
          createReadStream(abs).pipe(res)
          return
        }

        // ---- 编辑器宿主页 ----
        if (p.startsWith('/editor/')) {
          const claim = verifyToken(secret, p.slice('/editor/'.length))
          if (!claim || claim.k !== 'editor') return sendJson(403, { error: 'forbidden' })
          const entry = docs.get(String(claim.d || ''))
          if (!entry || !existsSync(entry.path)) return sendJson(404, { error: 'document not registered or file missing' })
          const html = renderEditorPage(entry)
          res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
          res.end(html)
          return
        }

        // ---- 保存回调 ----
        if (p.startsWith('/callback/')) {
          const docId = p.slice('/callback/'.length)
          handleCallback(req, res, docId, sendJson)
          return
        }

        // ---- 按路径打开（供右侧栏「用 OnlyOffice 编辑」按钮） ----
        if (p === '/open') {
          const raw = String(url.searchParams.get('path') || '').trim()
          if (!raw) return sendJson(400, { error: 'path 不能为空' })
          const abs = path.resolve(raw)
          if (!existsSync(abs) || !statSync(abs).isFile()) return sendJson(404, { error: `文件不存在：${abs}` })
          const ext = path.extname(abs).slice(1).toLowerCase()
          if (!DOC_TYPES[ext]) return sendJson(400, { error: `不支持的扩展名 .${ext}（支持：${OFFICE_EXTS.join(', ')}）` })
          const entry = registerDoc(abs)
          return sendJson(200, {
            ok: true, id: entry.id, path: abs, name: entry.name, editorUrl: editorUrlFor(entry),
          })
        }

        // ---- 在系统默认浏览器里打开（供右侧栏按钮） ----
        // 为什么不在浏览器侧直接 window.open：内嵌 webview 里那是一个静默的空操作，
        // 详见 launchBrowser 的注释。这里只接受本机存在的 office 文件，不接任意 URL。
        if (p === '/browse') {
          if (req.method !== 'POST') return sendJson(405, { error: '只接受 POST' })
          const raw = String(url.searchParams.get('path') || '').trim()
          if (!raw) return sendJson(400, { error: 'path 不能为空' })
          const abs = path.resolve(raw)
          if (!existsSync(abs) || !statSync(abs).isFile()) return sendJson(404, { error: `文件不存在：${abs}` })
          const ext = path.extname(abs).slice(1).toLowerCase()
          if (!DOC_TYPES[ext]) return sendJson(400, { error: `不支持的扩展名 .${ext}（支持：${OFFICE_EXTS.join(', ')}）` })
          const entry = registerDoc(abs)
          launchBrowser(editorUrlFor(entry)).then((launched) => {
            if (!launched.ok) return sendJson(500, { error: `没能拉起默认浏览器：${launched.error}` })
            return sendJson(200, { ok: true, name: entry.name })
          })
          return
        }

        // ---- 列表（UI） ----
        if (p === '/docs') {
          return sendJson(200, { docs: listDocs(), docsDir })
        }

        // ---- 上传（UI） ----
        if (p === '/upload') {
          handleUpload(req, res, url, sendJson)
          return
        }

        // ---- 健康摘要 ----
        if (p === '/status') {
          getText(`${docserverUrl}/healthcheck`, 8000).then((r) => {
            sendJson(200, {
              docserver: docserverUrl,
              docserverHealthy: r.status === 200 && /true/i.test(r.text),
              docPort: actualPort,
              docsDir,
              registered: docs.size,
            })
          }).catch((e) => sendJson(200, {
            docserver: docserverUrl, docserverHealthy: false, error: e.message, docPort: actualPort, docsDir, registered: docs.size,
          }))
          return
        }

        sendJson(404, { error: 'not found' })
      } catch (e) {
        // 服务必须扛过每一个异常：一个坏请求不该让文档服务挂掉
        try { sendJson(500, { error: e?.message || String(e) }) } catch { /* 响应已发出 */ }
      }
    })

    /**
     * 端口被占就顺延试下一个（最多 10 个），并把实际端口记进 actualPort。
     *
     * 为什么要有这一层：配置端口一旦被别的进程占着（本机实测过 3310 进入僵尸
     * FIN_WAIT_1 后长时间不可绑），旧实现只记一句日志就放弃 —— 工具照常注册、
     * 但 office_open 返回的链接指向一个没人监听的端口，用户看到的是「打不开」。
     * 顺延之后 URL、回调、状态行全部用 actualPort，行为自洽；DSH 本身不受影响。
     */
    let attempts = 0
    const tryListen = (port) => {
      const s = server
      if (!s) return
      const onError = (e) => {
        s.removeListener('listening', onListening)
        if ((e.code === 'EADDRINUSE' || e.code === 'EACCES') && attempts < 10) {
          attempts += 1
          const next = port + 1
          log('warn', `端口 ${port} 不可用（${e.code}），改试 ${next}`)
          tryListen(next)
          return
        }
        log('error', `文档服务启动失败（127.0.0.1:${port}）：${e.message}`)
        server = null
      }
      const onListening = () => {
        s.removeListener('error', onError)
        actualPort = port
        s.on('error', (err) => log('error', `文档服务异常：${err.message}`))
        log('info', `文档服务已监听 127.0.0.1:${actualPort}；文档目录 ${docsDir}；Document Server ${docserverUrl}`)
      }
      s.once('error', onError)
      s.once('listening', onListening)
      s.listen(port, '127.0.0.1')
    }
    tryListen(wantedPort)
  }

  function renderEditorPage(entry) {
    const ext = path.extname(entry.path).slice(1).toLowerCase()
    const key = computeDocKey(entry.path, entry.opens)
    entry.key = key
    const cfg = {
      documentType: docTypeFor(ext),
      width: '100%',
      height: '100%',
      document: {
        fileType: ext,
        key,
        title: entry.name,
        url: fetchUrlFor(entry.path),
        permissions: { edit: true, download: true, print: true },
      },
      editorConfig: {
        mode: 'edit',
        lang: 'zh',
        callbackUrl: `${baseUrlFor(actualPort)}/callback/${entry.id}`,
        // user 必须是稳定身份：同一个人重开文档时 DS 才认为「同一次协作」
        user: { id: 'dsh-user', name: 'DSH 用户' },
        customization: {
          forcesave: true,
          compactHeader: true,
          help: false,
          chat: false,
          comments: true,
          autosave: true,
        },
      },
    }
    const signed = { ...cfg }
    signed.token = signJwt(secret, cfg)
    const cfgJson = JSON.stringify(signed).replace(/</g, '\\u003c')
    return `<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(entry.name)}</title>
<script src="${docserverUrl}/web-apps/apps/api/documents/api.js"></script>
<style>html,body{margin:0;height:100%;overflow:hidden}#placeholder{height:100%}</style>
</head>
<body>
<div id="placeholder"></div>
<script>
var config = ${cfgJson};
config.events = {
  onError: function (e) { try { parent.postMessage({ source: 'dsh-office', type: 'error', detail: e }, '*') } catch (_) {} },
  onDocumentStateChange: function (e) { try { parent.postMessage({ source: 'dsh-office', type: 'dirty', dirty: !!(e && e.data) }, '*') } catch (_) {} },
  onDocumentReady: function () { try { parent.postMessage({ source: 'dsh-office', type: 'ready' }, '*') } catch (_) {} },
};
try { new DocsAPI.DocEditor('placeholder', config) } catch (e) { document.body.textContent = '编辑器加载失败：' + e; }
</script>
</body>
</html>`
  }

  function handleCallback(req, res, docId, sendJson) {
    readBody(req, 4 * 1024 * 1024).then(async (buf) => {
      let body = {}
      try { body = JSON.parse(buf.toString('utf8') || '{}') } catch { body = {} }
      const status = body.status
      const entry = docs.get(docId)
      log('info', `callback doc=${docId} status=${status} url=${body.url ? '有' : '无'}${entry ? '' : '（未知文档）'}`)

      // status: 1=编辑中 2=准备保存 3=保存出错 4=关闭无修改 6=强制保存 7=强制保存出错
      if (entry && (status === 2 || status === 6) && typeof body.url === 'string' && body.url) {
        try {
          await downloadTo(body.url, entry.path)
          // 保存成功后换 key：下次打开的是一次新的编辑器会话，不会命中旧缓存
          entry.opens += 1
          entry.savedAt = Date.now()
          log('info', `已写回 ${entry.path}（${statSync(entry.path).size} bytes）`)
        } catch (e) {
          log('error', `写回失败 ${entry.path}：${e.message}`)
        }
      }
      // 任何分支都必须回 error:0 —— 返回非 0 会让编辑器对用户弹「保存异常」
      sendJson(200, { error: 0 })
    }).catch((e) => {
      log('error', `回调读取失败：${e.message}`)
      try { sendJson(200, { error: 0 }) } catch { /* 已发出 */ }
    })
  }

  function handleUpload(req, res, url, sendJson) {
    const rawName = String(url.searchParams.get('name') || '').trim()
    const name = rawName ? path.basename(rawName) : `upload-${Date.now()}.docx`
    // 上传口与打开口用同一张扩展名表：否则会出现「文件躺在目录里、列表看得见，
    // 但编辑/浏览器两个按钮都报不支持」的鬼条目（真机教训 2026-11-05，pdf）。
    const ext = path.extname(name).slice(1).toLowerCase()
    if (!DOC_TYPES[ext]) {
      return sendJson(400, { error: `不支持的扩展名 .${ext}（支持：${OFFICE_EXTS.join(', ')}）` })
    }
    readBody(req, MAX_UPLOAD_BYTES).then((buf) => {
      if (!buf.length) return sendJson(400, { error: '空文件' })
      const target = uniquePath(docsDir, name)
      writeFileSync(target, buf)
      const entry = registerDoc(target)
      sendJson(200, { path: target, name: entry.name, editorUrl: editorUrlFor(entry), bytes: buf.length })
    }).catch((e) => sendJson(413, { error: e.message }))
  }

  function listDocs() {
    const out = []
    const seen = new Set()
    for (const entry of docs.values()) {
      if (!existsSync(entry.path)) continue
      const st = statSync(entry.path)
      seen.add(entry.path)
      out.push({
        id: entry.id, name: entry.name, path: entry.path,
        size: st.size, mtime: st.mtimeMs, editorUrl: editorUrlFor(entry),
        registered: true, openedAt: entry.openedAt, savedAt: entry.savedAt || null,
      })
    }
    // 文档目录里尚未在本进程打开过的文件也列出来（重启后仍可用）
    try {
      for (const name of readdirSync(docsDir)) {
        const abs = path.join(docsDir, name)
        if (seen.has(abs)) continue
        if (!OFFICE_EXTS.includes(path.extname(name).slice(1).toLowerCase())) continue
        let st
        try { st = statSync(abs) } catch { continue }
        if (!st.isFile()) continue
        const entry = registerDoc(abs)
        out.push({
          id: entry.id, name, path: abs, size: st.size, mtime: st.mtimeMs,
          editorUrl: editorUrlFor(entry), registered: true, openedAt: entry.openedAt, savedAt: null,
        })
      }
    } catch { /* 目录读不到就不列 */ }
    out.sort((a, b) => b.mtime - a.mtime)
    return out
  }

  // ---- Document Server 调用 ----

  async function docserverHealthy() {
    try {
      const r = await getText(`${docserverUrl}/healthcheck`, 8000)
      return r.status === 200 && /true/i.test(r.text)
    } catch { return false }
  }

  function requireHealthy() {
    return docserverHealthy().then((ok) => {
      if (!ok) throw new Error(`Document Server 不可达（${docserverUrl}）。请在「安装/卸载」里确认它已安装并运行。`)
    })
  }

  /** 调转换 API：返回输出文件路径。 */
  async function convertFile(absPath, outputtype) {
    const srcExt = path.extname(absPath).slice(1).toLowerCase()
    if (!srcExt) throw new Error(`无法判断源格式：${absPath}`)
    const key = createHash('sha256').update(`${absPath}|${statSync(absPath).mtimeMs}`).digest('hex').slice(0, 20)
    const payload = {
      async: false,
      filetype: srcExt,
      outputtype: String(outputtype).replace(/^\./, '').toLowerCase(),
      key,
      title: path.basename(absPath),
      url: fetchUrlFor(absPath),
    }
    payload.token = signJwt(secret, payload)
    const { json } = await postJson(`${docserverUrl}/converter`, payload)
    if (!json || typeof json !== 'object') throw new Error('转换响应格式异常')
    if (json.error) throw new Error(`转换失败 error=${json.error}（${describeConvertError(json.error)}）`)
    if (!json.fileUrl) throw new Error('转换响应里没有 fileUrl')
    const out = uniquePath(docsDir, `${basenameSansExt(absPath)}.${payload.outputtype}`)
    await downloadTo(json.fileUrl, out)
    return out
  }

  function describeConvertError(code) {
    const map = {
      '-1': '未知错误', '-2': '超时', '-3': '转换错误', '-4': '下载源文件出错（Document Server 取不到文件）',
      '-5': 'PDF 转换出错', '-6': '文件太大或参数异常', '-7': '未找到来源类型', '-8': '无效 token',
      '-9': '输出类型需要付费版', '-10': '请求参数错误', '-11': '磁盘空间不足',
    }
    return map[String(code)] || '参见官方错误码表'
  }

  /**
   * 用 /docbuilder（URL 方式）无头生成文档。
   *
   * ⚠ 10.0 也提供 /docbuilder/from-file（脚本直接进 multipart），但**本机实测走不通**：
   * 该端点额外要求 token 带 purpose=docbuilder-from-file 的 claim，缺了会返回
   * `{key, urls:{}, end:true}` —— 看着成功、其实一个文件都没产出（DS 日志里是
   * `builderFromFile auth failed {"code":-120,"description":"token:purpose"}`）。
   * 而 URL 方式 9.x/10.0 都稳定可用，所以这里**只走 URL 方式**：把脚本写进文档目录，
   * 给 Document Server 一个它取得到的绝对 URL。
   */
  async function generateDocument(filename, script) {
    const name = String(filename || '').trim() || `generated-${Date.now()}.docx`
    const outName = path.extname(name) ? name : `${name}.docx`
    const normalized = normalizeScript(script)

    const scriptPath = path.join(docsDir, `.gen-${Date.now()}-${randomBytes(4).toString('hex')}.js`)
    writeFileSync(scriptPath, normalized, 'utf8')
    const payload = { async: false, url: fetchUrlFor(scriptPath) }
    payload.token = signJwt(secret, payload)

    let json
    try {
      const r = await postJson(`${docserverUrl}/docbuilder`, payload, 180_000)
      json = r.json
    } finally {
      // 脚本只是传输载体：成功失败都清掉，别在文档目录里慢慢堆垃圾
      try { unlinkSync(scriptPath) } catch { /* 留着也无害 */ }
    }

    if (!json || typeof json !== 'object') throw new Error('生成响应格式异常')
    if (json.error) throw new Error(`生成失败 error=${json.error}`)

    const urls = json.urls || {}
    const names = Object.keys(urls)
    if (!names.length) {
      // 空 urls 是 DS 的静默失败：脚本没有按规范产出文件（最常见是被写成了多语句同行）
      throw new Error(
        'Document Server 没有产出任何文件（urls 为空）。'
        + '生成脚本必须满足：每条语句独立一行、以 builder.CreateFile(...) 开头、'
        + '以 builder.SaveFile(...) 与 builder.CloseFile() 结尾。'
        + `已规范化的脚本：\n${normalized.slice(0, 400)}`,
      )
    }
    const picked = names.find((n) => n === outName) || names[0]
    const target = uniquePath(docsDir, path.basename(picked) || outName)
    await downloadTo(urls[picked], target)
    return { path: target, files: names }
  }

  // ---- 工具 ----

  const tool = (name, description, parameters, execute) => ({
    name,
    description,
    parameters: { type: 'object', properties: parameters, required: Object.keys(parameters).filter((k) => k !== 'optional') },
    output: { schema: {}, render: (_a, v) => [{ type: 'text', text: JSON.stringify(v, null, 2) }] },
    execute,
  })

  function registerTools() {
    const disposers = []
    const specs = [
      tool('office_status',
        '查看 OnlyOffice 文档服务的状态：Document Server 是否可达、端口、文档目录、已打开文档数。排障或调用其他 office_* 工具前先跑它。',
        {},
        async () => {
          const healthy = await docserverHealthy()
          return {
            ok: healthy,
            docserver: docserverUrl,
            docserverHealthy: healthy,
            docPort: actualPort,
            docPortConfigured: wantedPort,
            docServiceUrl: baseUrlFor(actualPort),
            docsDir,
            registeredDocs: docs.size,
            registered: [...docs.values()].map((d) => ({ name: d.name, path: d.path, opens: d.opens })),
            hint: healthy ? null : 'Document Server 未运行：请在 DSH Desktop 的「安装/卸载」里确认 OnlyOffice 已安装且服务在运行',
          }
        }),

      tool('office_open',
        '用 OnlyOffice 打开一个本地 Office 文档（docx/xlsx/pptx 等）进行编辑，返回可直接在浏览器打开的编辑链接。'
        + '用户说「打开/编辑这份 Word/Excel/PPT」时用它。保存时内容会自动写回原文件。',
        { path: { type: 'string', description: '文档的本地绝对路径' } },
        async ({ path: p }) => {
          const abs = requireFile(p)
          const ext = path.extname(abs).slice(1).toLowerCase()
          if (!DOC_TYPES[ext]) throw new Error(`不支持的扩展名 .${ext}（支持：${OFFICE_EXTS.join(', ')}）`)
          await requireHealthy()
          const entry = registerDoc(abs)
          return {
            ok: true,
            path: abs,
            name: entry.name,
            editorUrl: editorUrlFor(entry),
            tip: '在浏览器打开 editorUrl 即可编辑；关闭或强制保存后内容自动写回原文件。',
          }
        }),

      tool('office_convert',
        '把一个 Office 文档转换成另一种格式（docx/xlsx/pptx → pdf/txt/csv/odt… 共 25+ 种），返回转换后文件的路径。'
        + '用户要「转成 PDF」「导出 CSV」时用它。',
        {
          path: { type: 'string', description: '源文档的本地绝对路径' },
          outputtype: { type: 'string', description: '目标格式，如 pdf / txt / csv / odt / pptx' },
        },
        async ({ path: p, outputtype }) => {
          if (!outputtype) throw new Error('outputtype 不能为空')
          const abs = requireFile(p)
          await requireHealthy()
          const out = await convertFile(abs, outputtype)
          return { ok: true, source: abs, output: out, bytes: statSync(out).size }
        }),

      tool('office_read_text',
        '把 Office 文档（docx/xlsx/pptx）转成纯文本后读回来，用于理解文档内容。'
        + '适合「这份文档讲了什么」「读一下这个 Excel」这类需要真正看到内容的请求。'
        + '正文超过 2 万字符时会落盘，只返回路径与预览。',
        { path: { type: 'string', description: '文档的本地绝对路径' } },
        async ({ path: p }) => {
          const abs = requireFile(p)
          await requireHealthy()
          const txtPath = await convertFile(abs, 'txt')
          const text = stripBom(readFileSync(txtPath, 'utf8'))
          const chars = text.length
          if (chars <= MAX_TEXT_CHARS) {
            return { ok: true, source: abs, chars, text }
          }
          // 长正文不过 stdio：几 MB 的工具结果是最快把会话撑坏的方式
          return {
            ok: true, source: abs, chars, truncated: true,
            path: txtPath,
            preview: text.slice(0, MAX_TEXT_CHARS),
            tip: `正文 ${chars} 字符，已落盘到 path；用 read 工具按需读取。`,
          }
        }),

      tool('office_forcesave',
        '对一份当前正被 OnlyOffice 打开的文档执行强制保存（forcesave），把编辑中的内容立刻落盘。'
        + 'key 来自 office_open/office_status 的返回。用户说「保存一下」「存盘」时用它。',
        { key: { type: 'string', description: '编辑器会话的 document.key' } },
        async ({ key }) => {
          if (!key) throw new Error('key 不能为空')
          await requireHealthy()
          const payload = { c: 'forcesave', key }
          payload.token = signJwt(secret, payload)
          const { json } = await postJson(`${docserverUrl}/command`, payload)
          return { ok: !json?.error, response: json, tip: json?.error ? 'key 失效或文档未打开；请先用 office_open 重新打开' : null }
        }),

      tool('office_generate',
        '用 OnlyOffice 文档生成器无头生成一份 docx/xlsx/pptx（不需要打开编辑器）。'
        + 'script 是一段 Office API JavaScript，例如：'
        + 'builder.CreateFile("docx"); var d=Api.GetDocument(); d.GetElement(0).AddText("你好"); builder.SaveFile("docx","报表.docx"); builder.CloseFile();',
        {
          filename: { type: 'string', description: '输出文件名，如 周报.docx' },
          script: { type: 'string', description: 'Office API JavaScript 生成脚本（以 builder.CreateFile 开头、builder.CloseFile 结尾）' },
        },
        async ({ filename, script }) => {
          if (!script) throw new Error('script 不能为空')
          await requireHealthy()
          const out = await generateDocument(filename, script)
          return { ok: true, ...out, bytes: statSync(out.path).size }
        }),
    ]

    for (const spec of specs) {
      try {
        disposers.push(ctx.tools.register(spec))
      } catch (e) {
        // 「工具在 manifest 里但 AI 看不到」的唯一线索，必须显眼
        log('error', `注册工具 ${spec.name} 失败：${e?.message || e}`)
      }
    }
    log('info', `已注册 ${disposers.length}/${specs.length} 个工具`)
    return () => { for (const d of disposers) { try { d() } catch { /* 已卸载 */ } } }
  }

  // ctx.effect：返回值是清理函数，插件停用/DSH 退出时由 Cordis 调用
  return ctx.effect(async () => {
    let unregisterTools = () => {}
    try {
      startServer()
      unregisterTools = registerTools()
      // 启动自检：Document Server 不通只记日志，不影响 DSH
      docserverHealthy().then((ok) => {
        log(ok ? 'info' : 'warn', ok
          ? `Document Server 健康检查通过（${docserverUrl}）`
          : `Document Server 不可达（${docserverUrl}）——工具会报错，DSH 照常可用`)
      })
    } catch (e) {
      log('error', `启动失败：${e?.message || e}`)
    }
    return () => {
      try { unregisterTools() } catch { /* 已卸载 */ }
      if (server) {
        try { server.close() } catch { /* 已关 */ }
        server = null
      }
    }
  })
}

/** 工具注册与系统提示词都不需要额外服务；只要 tools。 */
export const inject = ['tools']

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]))
}

// 模块可加载性自检（DSH_OFFICE_BRIDGE_SELFTEST=1 时跑）：
// 能走到这里就说明顶层依赖全部解析成功——py-bridge 那条「import 写错、整包静默消失」
// 的故障，靠这一条兜底。
if (process.env.DSH_OFFICE_BRIDGE_SELFTEST === '1') {
  const failures = []
  if (typeof createServer !== 'function') failures.push('node:http createServer 不可用')
  if (typeof apply !== 'function') failures.push('缺少 export apply')
  if (!Array.isArray(inject) || !inject.includes('tools')) failures.push("inject 必须含 'tools'")
  if (failures.length) {
    console.error('[office-bridge] 自检失败:\n  ' + failures.join('\n  '))
    process.exitCode = 1
  } else {
    console.log('[office-bridge] 自检通过：模块可加载、插件契约完整')
  }
}
