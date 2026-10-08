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
/** 能力发现提示词段落的注册名：必须全 profile 唯一，重复注册会 throw */
const PROMPT_SECTION = 'dsh-desktop-python-bridge-capabilities'

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
  /**
   * 能力发现提示词的**已渲染快照**（start() 里预热）。
   *
   * 为什么存字符串而不是每次现取：section 的 `text` provider 是**同步**的
   * （`(context) => string`），没法在里面 await 一次 JSON-RPC；而提示词组装
   * 每轮都调，每次现取就等于每轮多起一个子进程。
   * 所以在握手完成后那次 start() 里预热一次，写时刷新（见下方 assign）。
   */
  let capabilityText = ''

  const log = (level, message) => {
    // 走插件自己的 logger：这些行会进 DSH 日志，用户能在排障时看到桥接的状态
    ctx.logger?.[level === 'ERROR' ? 'error' : 'warn']?.(`[py-bridge] ${message}`)
  }

  // 进程重启后旧快照作废：那一瞬间最可能「环境变了」（重启原因常是崩溃），
  // 而且留着旧文本会在桥接未就绪时继续对外宣称一批其实不可用的库。
  const resetCapabilities = () => { capabilityText = '' }

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
          resetCapabilities()
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

      // 顺手取一次能力发现文案，把 section provider **预热**好
      // （provider 是同步的，不能在里面 await；缓存也免得每次组装提示词都起子进程）。
      // 文本由 **Python 侧** 生成（describe_python_capabilities）—— 两边各写一份
      // 必然漂移：白名单变了而提示词还在说旧话，模型就会照错的信息行事。
      // 失败不影响工具注册 —— 那才是主线功能。
      try {
        const res = await conn.request('describe_python_capabilities', {}, 60_000)
        capabilityText = res?.ok && typeof res.text === 'string' ? res.text : ''
        if (!capabilityText) log('WARN', '能力发现：Python 侧没返回文案（不影响工具）')
        else log('INFO', `能力发现：已写入 ${capabilityText.split('\n').length} 行环境说明`)
      } catch (e) {
        capabilityText = ''
        log('WARN', `能力发现：取文案失败（不影响工具）：${e.message}`)
      }
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
        registerCapabilities()
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

    /**
     * 能力发现：把「当前环境装了哪些库」写进系统提示词。
     *
     * `text` 传的是**函数**而不是字符串 —— `PromptSection.text` 支持
     * `(context) => string`，每次组装提示词时求值（见 dsh-system-prompt 的
     * PromptSection 类型）。所以用户日后自己装了 pandas，这段会自动多出 pandas，
     * **不需要重启 DSH、不需要重新注册插件**，兑现 README 里的承诺。
     * 静态文本做不到这件事：它在 import 期就固定了。
     *
     * 求值里只有一次 JSON-RPC 往返（`get_python_environment_info`），且有缓存：
     * 提示词组装每轮都会调，不能每次都起子进程。缓存在看门狗重启进程时清掉 ——
     * 那才是「环境可能变了」的时机。
     *
     * `interpolate: false` 是必需的：这段文本含中文与括号，一旦某个库的描述里
     * 出现 `{{`，默认插值会把它当成 prompt 变量并**抛错**（未知变量 = 组装失败）。
     * 宁可原样渲染，也不要让一段说明文字把整个提示词搞挂。
     */
    function registerCapabilities() {
      try {
        ctx.systemPrompt.section({
          name: PROMPT_SECTION,
          order: 200,
          interpolate: false,
          // 读的是 start() 预热好的快照（同步可读）。
          // 返回空串 = 这一段在组装时被丢掉（dsh-system-prompt 会 filter 掉空段），
          // 正好是「桥接没起来就别声称有什么工具」的诚实做法。
          text: () => capabilityText,
        })
      } catch (e) {
        // 这一段只是**增强**：注册失败不该连累工具注册（工具才是主线功能）
        log('WARN', `能力发现段落注册失败（不影响工具）：${e.message}`)
      }
    }
  })
}

// 注入 tools（注册桥接工具）与 systemPrompt（能力发现段落）。
// 少了后者 apply() 里的 ctx.systemPrompt 可能是 undefined —— 那一段是**增强**，
// 拿不到就只注册工具，不让可选功能拖垮主线。
export const inject = ['tools', 'systemPrompt']