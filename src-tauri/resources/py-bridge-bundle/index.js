// DSH Desktop —— Python 能力桥接：Host 插件（生成文件，请勿手工编辑）
//
// 职责：spawn `python -u -m dsh_bridge.runtime <entry>` → NDJSON JSON-RPC 握手拿
// manifest → 把 manifest.tools[] 注册进 ctx.tools → tool call 转发回 Python。
//
// 为什么需要这一层：本机 DSH 安装里**没有**现成的 python-bridge 消费方
// （`@deepseek-ai/dsh-python-bridge-codegen` 那条「免 codegen」路子只随 Python
// 侧发布），所以宿主这一端必须自己写。约 200 行，换来 AI 直接能用 markitdown。
//
// 生命周期纪律：子进程归本插件管 —— ctx.effect 里注册的一切在插件卸载时统一清理，
// 看门狗负责崩溃重启；DSH 退出时插件被卸载 → 子进程被 kill，**不留孤儿进程**。

import { spawn } from 'node:child_process'
import { defineTool } from '@deepseek-ai/dsh-tools'

// 握手用的客户端版本。**major 必须与 Python 侧 dsh_bridge.__version__ 的 major 相同**，
// 否则 runtime 会以 protocol-mismatch 拒绝（-32006）。这里刻意不硬读版本号：
// 由握手失败时的报错文案告诉你该装哪个版本，比我们自己猜一个更可靠。
const CLIENT_VERSION = '0.0.1'

/** 崩溃重启的退避：第 n 次失败后等 n 秒，上限 10 秒，最多 5 次。 */
const MAX_RESTARTS = 5
const HANDSHAKE_TIMEOUT_MS = 60_000
const CALL_TIMEOUT_MS = 10 * 60_000

/** runtime 把「没装 / 版本不匹配」这类问题映射成的错误码（见 dsh_bridge/_errors.py）。 */
const CODE_TIMEOUT = -32001
const CODE_PERMISSION = -32003

/**
 * 一个 NDJSON JSON-RPC 连接：自己解析行、写请求、匹配 id。
 *
 * 没有 import `@deepseek-ai/dsh-sdk-protocol` 的 JsonRpcLineTransport：
 * 那个传输层把「请求处理函数」也接进来了，而这里的方向恰好相反（我们要**发**请求、
 * 只**收**响应），自己写 40 行比硬套它更直白，也少一条运行时依赖。
 */
class BridgeConnection {
  #child = null
  #pending = new Map()
  #buffer = ''
  #nextId = 1
  #onLog = () => {}
  #onExit = () => {}

  /**
   * @param {import('node:child_process').ChildProcess} child
   * @param {{ onLog?: (level: string, message: string) => void, onExit?: (code: number | null, signal: string | null) => void }} hooks
   */
  constructor(child, hooks = {}) {
    this.#child = child
    this.#onLog = hooks.onLog ?? (() => {})
    this.#onExit = hooks.onExit ?? (() => {})
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (chunk) => this.#onData(chunk))
    child.stderr.setEncoding('utf8')
    child.stderr.on('data', (chunk) => {
      const text = String(chunk).trim()
      if (text) this.#onLog('WARN', `[py-bridge stderr] ${text}`)
    })
    child.on('exit', (code, signal) => {
      // 进程没了，所有在途请求都不可能再有响应 —— 立刻 reject，
      // 否则调用方会一直等到 CALL_TIMEOUT_MS（10 分钟）才看到超时。
      const err = new Error(`Python 桥接进程已退出（code=${code} signal=${signal}）`)
      for (const { reject } of this.#pending.values()) reject(err)
      this.#pending.clear()
      this.#onExit(code, signal)
    })
  }

  #onData(chunk) {
    this.#buffer += chunk
    let idx
    while ((idx = this.#buffer.indexOf('\n')) >= 0) {
      const line = this.#buffer.slice(0, idx).trim()
      this.#buffer = this.#buffer.slice(idx + 1)
      if (!line) continue
      let msg
      try {
        msg = JSON.parse(line)
      } catch {
        // runtime 保证 stdout 纯净（用户代码的 print 被代理成 bridge/log 通知），
        // 这里出现非 JSON 行说明协议被破坏了 —— 记日志并继续，不让一行噪声拖垮连接。
        this.#onLog('WARN', `[py-bridge] 收到非 JSON 行：${line.slice(0, 200)}`)
        continue
      }
      this.#dispatch(msg)
    }
  }

  #dispatch(msg) {
    // Python → 宿主 的通知（bridge/log：Python 侧的 logging 被路由回来）
    if (msg.method === 'bridge/log') {
      const { level = 'INFO', message = '' } = msg.params ?? {}
      this.#onLog(level, `[py-bridge] ${message}`)
      return
    }
    if (msg.id === undefined) return
    const entry = this.#pending.get(msg.id)
    if (!entry) return
    this.#pending.delete(msg.id)
    if (msg.error) {
      const e = new Error(msg.error.message || `Python 桥接错误 ${msg.error.code}`)
      e.code = msg.error.code
      e.kind = msg.error.data?.kind
      entry.reject(e)
    } else {
      entry.resolve(msg.result)
    }
  }

  /**
   * 发一个 JSON-RPC 请求并等响应。
   * @param {string} method
   * @param {object} params
   * @param {number} timeoutMs
   */
  request(method, params, timeoutMs = CALL_TIMEOUT_MS) {
    if (!this.#child || this.#child.exitCode !== null) {
      return Promise.reject(new Error('Python 桥接进程未运行'))
    }
    const id = this.#nextId++
    const payload = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n'
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id)
        const e = new Error(`Python 桥接调用超时：${method}`)
        e.code = CODE_TIMEOUT
        reject(e)
      }, timeoutMs)
      this.#pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v) },
        reject: (e) => { clearTimeout(timer); reject(e) },
      })
      this.#child.stdin.write(payload, (err) => {
        if (err) {
          this.#pending.delete(id)
          clearTimeout(timer)
          reject(err)
        }
      })
    })
  }

  kill() {
    if (this.#child && this.#child.exitCode === null) {
      try { this.#child.kill() } catch { /* 已经死了就算了 */ }
    }
  }
}

/** runtime 侧报「这个文件不是模块」/「找不到模块」时的兜底提示。 */
function describeStartupFailure(err, pythonPath, cwd) {
  const raw = String(err?.message ?? err)
  if (/Cannot find module|No module named/i.test(raw)) {
    return `Python 侧缺少 dsh_bridge（解释器 ${pythonPath} 没装 dsh-python-bridge），` +
      `或入口模块不在 ${cwd}。请在 DSH 首选项「Python 环境」里重新点一次「基本安装」。`
  }
  if (/protocol-mismatch|-32006/i.test(raw)) {
    return `dsh-python-bridge 版本与宿主不匹配（客户端 ${CLIENT_VERSION}）：${raw}`
  }
  if (/ENOENT|not found/i.test(raw) && /spawn|pythonPath/i.test(raw + String(pythonPath))) {
    return `找不到 Python 解释器 ${pythonPath}，请在首选项「Python 环境」里重新检测。`
  }
  return raw
}

/** 把 Python 工具的返回值包装成 DSH 需要的 ContentBlock。 */
function toContentBlocks(value) {
  if (typeof value === 'string') return [{ type: 'text', text: value }]
  try {
    return [{ type: 'text', text: JSON.stringify(value, null, 2) }]
  } catch {
    return [{ type: 'text', text: String(value) }]
  }
}

/**
 * 插件主体。
 *
 * 导出形式按 host-plugin.md 的规定二选一：这里用 `export function apply(ctx, config)`
 * + `inject: ['tools']`（需要 ctx.tools 才能注册工具）。
 *
 * @param {import('@deepseek-ai/dsh').Context} ctx
 * @param {{ pythonPath?: string, entryModule?: string, bridgeDir?: string, cwd?: string }} config
 */
export function apply(ctx, config = {}) {
  const pythonPath = (config.pythonPath ?? '').trim()
  const entryModule = (config.entryModule ?? 'bridge_entry').trim() || 'bridge_entry'
  const bridgeDir = (config.bridgeDir ?? '').trim()
  const cwd = (config.cwd ?? '').trim() || bridgeDir

  // 没装桥接就安静地什么都不做：DSH 必须照常可用（插件不是 DSH 的启动依赖）
  if (!pythonPath || !bridgeDir) {
    ctx.logger?.info?.('[py-bridge] 未配置 pythonPath/bridgeDir，跳过注册（请先执行「基本安装」）')
    return () => {}
  }

  /** @type {BridgeConnection | null} */
  let conn = null
  let disposed = false
  let restartCount = 0
  let starting = null
  /** @type {Array<{ description: string, parameters: object }>} */
  const manifestTools = []

  const log = (level, message) => {
    // 走插件自己的 logger：这些行会进 DSH 日志，用户能在排障时看到桥接的状态
    ctx.logger?.[level === 'ERROR' ? 'error' : 'warn']?.(`[py-bridge] ${message}`)
  }

  /**
   * spawn 子进程并完成 initialize 握手。
   *
   * `-u` 绝不能省：Python 在管道里会块缓冲 stdout，缺了它握手响应会卡在缓冲区里，
   * 表现为「插件装了但一个工具都没有」。
   */
  async function start() {
    if (disposed) return
    if (starting) return starting

    starting = (async () => {
      const child = spawn(
        pythonPath,
        ['-u', '-m', 'dsh_bridge.runtime', entryModule],
        {
          cwd,
          windowsHide: true,
          stdio: ['pipe', 'pipe', 'pipe'],
          env: {
            ...process.env,
            // 桥接产物目录：Python 侧落盘长结果时用得到
            DSH_PY_BRIDGE_HOME: bridgeDir,
            PYTHONIOENCODING: 'utf-8',
            PYTHONUTF8: '1',
          },
        },
      )
      conn = new BridgeConnection(child, {
        onLog: log,
        onExit: (code, signal) => {
          if (disposed) return
          restart()
          if (code !== 0) log('ERROR', `桥接进程退出（code=${code} signal=${signal}）`)
        },
      })

      const handshake = conn.request(
        'initialize',
        { clientInfo: { name: 'dsh-desktop-python-bridge', version: CLIENT_VERSION } },
        HANDSHAKE_TIMEOUT_MS,
      )
      let result
      try {
        result = await handshake
      } catch (err) {
        conn.kill()
        conn = null
        throw new Error(describeStartupFailure(err, pythonPath, cwd))
      }

      const manifest = result?.manifest ?? {}
      manifestTools.length = 0
      for (const t of manifest.tools ?? []) {
        manifestTools.push({
          description: t.description,
          parameters: t.parameters ?? { type: 'object', properties: {} },
        })
      }
      restartCount = 0
      log('INFO', `握手完成：server ${result?.serverInfo?.name} ${result?.serverInfo?.version}，` +
        `${manifestTools.length} 个工具已就绪`)
    })()

    try {
      await starting
    } finally {
      starting = null
    }
  }

  /** 崩溃后的指数退避重启；超过次数就不再重试（避免无限 fork）。 */
  function restart() {
    if (disposed || restartCount >= MAX_RESTARTS) return
    const delay = Math.min(1000 * 2 ** restartCount, 10_000)
    restartCount += 1
    log('WARN', `桥接进程异常退出，${delay / 1000}s 后重启（第 ${restartCount}/${MAX_RESTARTS} 次）`)
    setTimeout(() => { start().catch((e) => log('ERROR', `重启失败：${e.message}`)) }, delay)
  }

  /**
   * 把 manifest 里的每个工具包成 ctx.tools 能注册的 definition。
   *
   * execute 里做的是「把 args 原样转发」：参数校验交给 Python 侧（它的签名才是真的），
   * 这里再校验一遍只会引入两份可能不一致的规则。
   */
  function toolDefinitions() {
    return manifestTools.map((t) => defineTool({
      name: t.name,
      description: t.description,
      parameters: t.parameters,
      output: {
        // 桥接工具的返回值形状不统一（{ok, result} / {path, preview} / {ok, error}），
        // 这里统一成一个宽松的 JSON 值输出，不做强制 schema —— 强制了必然误伤。
        schema: { type: 'json' },
        render: (_args, value) => toContentBlocks(value),
      },
      execute: async (args) => {
        if (!conn) {
          // 握手失败过或进程已死：现拉一次再试，而不是直接报错给用户
          await start()
        }
        if (!conn) throw new Error('Python 桥接不可用')
        return await conn.request(t.name, args ?? {}, CALL_TIMEOUT_MS)
      },
    }))
  }

  // ctx.effect：插件停用/卸载时自动执行 —— 杀子进程、复位引用。
  // 这条承诺是「DSH 关闭时 Python 桥接不残留」的落点。
  return ctx.effect(() => {
    disposed = true
    if (conn) conn.kill()
    conn = null
    return () => {}
  }).after(() => {
    // 首启：立刻注册（此时 manifestTools 还是空的），
    // 握手完成后用新 manifest 重新注册一次 —— 见下方 unregister/重新 register。
    let unregister = () => {}
    start()
      .then(() => {
        if (disposed) return
        unregister = registerAll()
      })
      .catch((e) => log('ERROR', `启动失败：${e.message}`))

    function registerAll() {
      const disposers = []
      for (const def of toolDefinitions()) {
        const handle = ctx.tools.register(def)
        disposers.push(() => handle.unregister?.())
      }
      return () => { for (const d of disposers) { try { d() } catch { /* 已卸载 */ } } }
    }
  })
}

export const inject = ['tools']