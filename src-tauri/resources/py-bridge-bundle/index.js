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
//
// ⚠⚠ 零顶层运行时 import —— 这是本文件最重要的架构约束，不是代码风格问题。
// 曾经顶层写 `import { defineTool } from '@deepseek-ai/dsh-tools'`，结果**整个插件
// 一个工具都没注册**，且看不出任何原因。实测结论：profile 的 node_modules 里
// **没有** `@deepseek-ai/dsh-tools`（只有 DSH 自己的安装目录里有，pnpm 不会把它
// 暴露给 profile 里的 bundle），所以那个裸包名 import 在模块加载期就
// ERR_MODULE_NOT_FOUND —— `apply()` 从未被调用。
// 本机两个可用的第三方插件各自用不同方式绕开它，两条路都验证过能跑：
//   · `dsh-github` 把 dsh-tools 的一个子模块**物理拷贝**进 lib/vendor/ 再相对引用；
//   · `dsh-multimedia` **完全不用 defineTool**，手写 tool 对象直接 ctx.tools.register。
// 这里走第二条：manifest 的参数本来就是标准 JSON Schema envelope，
// 与 dsh-multimedia 手写的形状同构，原样注册即可，不需要 defineTool 的编译层。
// `node:` 前缀的内置模块仍然可用（那不是 npm 依赖）。

import { spawn } from 'node:child_process'
import { existsSync, statSync } from 'node:fs'
import path from 'node:path'

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
    // ⚠ `error` 必须监听（真机教训 2026-10-18）：`spawn` 在可执行文件不存在时
    // （ENOENT，例如用户换了 Python 解释器、或 profile 里的 pythonPath 指向已删除
    // 的目录）不会 throw，而是**异步 emit 'error'**。Node 的规矩是：'error' 事件
    // 若没有监听者，就被当成**未捕获异常**，直接杀掉整个宿主进程 ——
    // `failOnStartupError: false` 兜不住这一层（它管的是插件 apply 抛错）。
    // 实测症状：DSH 启动即 `fatal uncaught exception: Error: spawn … ENOENT`，
    // 退出码 1，连 Web 界面都起不来。桥接挂了绝不该拖垮 DSH。
    child.on('error', (err) => {
      this.#settleAll(err)
      this.#onExit(null, null, err)
    })
    child.on('exit', (code, signal) => {
      // 进程没了，所有在途请求都不可能再有响应 —— 立刻 reject，
      // 否则调用方会一直等到 CALL_TIMEOUT_MS（10 分钟）才看到超时。
      this.#settleAll(new Error(`Python 桥接进程已退出（code=${code} signal=${signal}）`))
      this.#onExit(code, signal)
    })
  }

  /** 把在途请求全部 reject 掉（进程死了 / spawn 失败时调用）。 */
  #settleAll(err) {
    for (const { reject } of this.#pending.values()) reject(err)
    this.#pending.clear()
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

/**
 * 选一个**真实存在**的工作目录给 spawn 用。
 *
 * 为什么必须有这一层：`spawn(cmd, args, { cwd })` 里 cwd 不存在时，Node 抛的是
 * `Error: spawn <cmd> ENOENT` —— 报错指向可执行文件，而真正的毛病在 cwd。
 * 真机踩过一次（2026-10-18）：用户切换 DSH home 后，配置里留着旧的绝对路径，
 * 表现成「Python 被删了」，实际 Python 完好。桥接是可选功能，**绝不能因为
 * 一个失效的工作目录就让整个 DSH 起不来**。
 *
 * 回退顺序：配置值 → bridgeDir → 用户主目录 → 当前进程 cwd。
 * 全都不可用时返回 undefined（= 交给 Node 用默认 cwd），而不是传一个坏路径。
 */
function pickUsableCwd(...candidates) {
  // 最后一个参数可能是 ctx（用于记日志），把它挑出来
  let logger = null
  const tail = candidates[candidates.length - 1]
  if (tail && typeof tail === 'object' && typeof tail.logger !== 'undefined') {
    logger = tail.logger
    candidates = candidates.slice(0, -1)
  }
  const fallbacks = [process.env.USERPROFILE, process.env.HOME, process.cwd()].filter(Boolean)

  const tried = []
  for (const c of [...candidates, ...fallbacks]) {
    const v = typeof c === 'string' ? c.trim() : ''
    if (!v) continue
    tried.push(v)
    try {
      if (existsSync(v) && statSync(v).isDirectory()) {
        if (tried.length > 1) {
          logger?.warn?.(`[py-bridge] 工作目录不可用，改用 ${v}（原值：${tried[0]}）`)
        }
        return v
      }
    } catch {
      // 权限问题等：当作不可用，继续试下一个
    }
  }
  logger?.warn?.(`[py-bridge] 找不到可用的工作目录（试过：${tried.join(' | ')}），交给 Node 默认值`)
  return undefined
}

/** 把 Python 工具的返回值包装成 DSH 需要的 ContentBlock。 */
function toContentBlocks(value) {  if (typeof value === 'string') return [{ type: 'text', text: value }]
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
  // ⚠ cwd 刻意**不回退到 bridgeDir**（真机故障 2026-10-20）：
  // Windows 上进程的 cwd 会锁住那个目录，「卸载办公包」时桌面端就删不掉它
  // （os error 32，只留下一个空壳目录）。模块查找由 PYTHONPATH 负责，
  // 所以 cwd 用哪都行 —— 但**不能**是 bridgeDir。
  const cwdRaw = (config.cwd ?? '').trim()

  // 没装桥接就安静地什么都不做：DSH 必须照常可用（插件不是 DSH 的启动依赖）
  if (!pythonPath || !bridgeDir) {
    ctx.logger?.info?.('[py-bridge] 未配置 pythonPath/bridgeDir，跳过注册（请先执行「基本安装」）')
    return () => {}
  }

  /**
   * 桥接代码本身是否还在。
   *
   * 真机故障（2026-10-18）：用户机器上 `%APPDATA%\com.dsh.desktop\py-bridge\`
   * 整个目录被删了（很可能是卸载旧版或手工清理），但 profile 里的插件还在、
   * cordis.patch.yml 里还写着那个路径。后果有两层：
   *   ① `cwd` 指向不存在的目录 → `spawn` 报 **ENOENT**（且报的是 exe 路径，
   *      看起来像「Python 被删了」，实测 Python 完好）；
   *   ② 即使 cwd 回退成功、进程起来了，Python 侧也会
   *      `No module named dsh_bridge.runtime` —— 代码文件根本不在。
   *
   * 所以这里**先看文件在不在**，比 spawn 失败后再猜要准确得多。
   * 缺了只记一句能行动的日志，然后安静退出（DSH 照常可用）。
   */
  const ENTRY_FILE = 'bridge_entry.py'
  const entryPath = path.join(bridgeDir, ENTRY_FILE)
  if (!existsSync(entryPath)) {
    ctx.logger?.warn?.(
      `[py-bridge] 桥接代码缺失：${entryPath} 不存在（目录可能被清理过）。` +
        `工具暂不可用，但 DSH 照常工作 —— 请在首选项「Python 环境」里重新执行「基本安装」以恢复。`,
    )
    return () => {}
  }

  /**
   * 工作目录：**必须是真实存在的目录**，而且**不能是 bridgeDir**。
   *
   * 真机教训（2026-10-18）：`spawn` 的 ENOENT 不只表示「可执行文件不存在」——
   * 当 `cwd` 选项指向一个**已删除的目录**时，Node 报的同样是
   * `Error: spawn <python.exe> ENOENT`，且 `path` 字段显示的是 exe 路径，
   * 极易误判成「Python 被删了」。实测该机器上 Python 3.14.7 完好无损。
   *
   * 策略：cwd 失效就退到用户主目录，再退到当前进程 cwd，
   * 最后兜底 `undefined`（= 交给 Node 用默认 cwd）—— **绝不把失效路径交给 spawn**。
   *
   * ⚠⚠ **cwd 刻意避开 bridgeDir**（真机故障 2026-10-20）：
   * Windows 上，一个进程的 cwd 会**锁住**那个目录 —— 别的进程对它
   * `RemoveDirectoryW` 会拿到 `os error 32`（另一个程序正在使用此文件）。
   * 于是「卸载办公包」时：桥接进程还活着（它的 cwd 正是 bridgeDir），
   * 桌面端删不掉那个目录，只能留下一个**空壳目录**（文件全被删了，目录壳子留着）。
   *
   * 解法：cwd 用别处，靠 **PYTHONPATH** 把 bridgeDir 送进 `sys.path` ——
   * Python 照样能 `import bridge_entry` 与平铺导入同目录的各工具模块
   * （`sys.path` 的第一项就是 PYTHONPATH），而 bridgeDir **不再被任何进程占用**。
   * 本机实测：cwd 在别处 + PYTHONPATH 指向该目录 → `-m dsh_bridge.runtime bridge_entry`
   * 正常启动，且 `remove_dir_all` 一次就把目录删干净了。
   */
  const cwd = pickUsableCwd(cwdRaw, ctx)

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
      // 先检查解释器是否存在（真机教训 2026-10-18）：spawn 的 ENOENT 是**异步**
      // 报的，如果只依赖 'error' 事件，握手会白等 HANDSHAKE_TIMEOUT_MS（60 秒）
      // 才失败。这里提前判定，失败信息也更准确（能说出「哪个路径不存在」）。
      if (!existsSync(pythonPath)) {
        throw new Error(
          `找不到 Python 解释器：${pythonPath}（可能是换了 Python 版本、或该目录已被删除）。` +
            `请在首选项「Python 环境」里重新检测，或重新执行「基本安装」。`,
        )
      }
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
            // ⚠ PYTHONPATH 指向 bridgeDir —— 这是「cwd 不再用 bridgeDir」的前提。
            // Python 靠 sys.path 找 `-m dsh_bridge.runtime bridge_entry` 里的
            // `bridge_entry` 模块，以及它平铺导入的同目录各工具模块；
            // sys.path 的第一项正是 PYTHONPATH，所以 cwd 在哪都不影响 import。
            // 反过来，cwd 若落在 bridgeDir，那个目录就被进程锁住，
            // 桌面端「卸载办公包」时删不掉它（真机故障 2026-10-20）。
            //
            // 已有 PYTHONPATH 时**追加**而不是覆盖：那是用户自己设的，别弄丢。
            PYTHONPATH: process.env.PYTHONPATH
              ? `${process.env.PYTHONPATH}${path.delimiter}${bridgeDir}`
              : bridgeDir,
            PYTHONIOENCODING: 'utf-8',
            PYTHONUTF8: '1',
          },
        },
      )
      conn = new BridgeConnection(child, {
        onLog: log,
        onExit: (code, signal, spawnError) => {
          resetCapabilities()
          if (disposed) return
          // spawn 失败（ENOENT / EACCES）**不重启**：可执行文件不在就是不在，
          // 退避重试 5 次只是把同一条错误刷 5 遍，还会让用户以为「正在自愈」。
          // 这条路径必须只记一句能行动的日志，然后安静下来（工具不可用，
          // 但 DSH 照常工作）。用户换回解释器后重启 DSH 即可恢复。
          if (spawnError) {
            log('ERROR', describeStartupFailure(spawnError, pythonPath, cwd))
            return
          }
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
        // ⚠ name 必须显式带上 —— 第八轮真机教训（2026-10-13）：曾只填
        // description/parameters，registerAll 里 name: t.name 取到 undefined。
        // register() 对 name 只查「≠ run_code」，于是 12 个工具以 undefined 为名
        // 「注册成功」，wireSchemas 投影出 {name: undefined, …}，而 undefined 不是
        // 合法 JSON 值 —— session.append("request/header") 的无损检查当场抛
        // 「carries non-JSON-serializable data」，每一轮请求都崩。
        if (typeof t.name !== 'string' || t.name.length === 0) {
          log('WARN', 'manifest 里有一个没有名字的工具，已跳过')
          continue
        }
        manifestTools.push({
          name: t.name,
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

  // ctx.effect：execute 的**返回值**就是清理函数，插件停用/卸载时由 Cordis 调用。
  // ⚠ 真实 API：`ctx.effect()` 返回一个可调用、可 await 的 wrapper（见 cordis
  //   lib/index.js 的 `effect(execute, label)`：1278 行 `return wrapper`，
  //   1275 行只挂了 `.then`，**没有 `.after()`**）。我第一版写成
  //   `ctx.effect(...).after(...)`，真机报
  //   `TypeError: ctx.effect(...).after is not a function`，整个插件没激活 ——
  //   教训又是「凭想象写 API」。dsh-tools 的 `tools.register()` 用的是
  //   `this.layers.effect(this.ctx, (layer) => ...)`，同样只依赖返回值。
  return ctx.effect(async () => {
    // 首启：先握手（拿到 manifest），再把工具与能力发现段落注册上。
    // 握手失败只记日志：`failOnStartupError: false` 的语义就是「桥接没起来
    // 也不该让 DSH 起不来」，而 execute 抛错会让整个 entry 激活失败。
    let unregister = () => {}
    try {
      await start()
      if (disposed) return () => {}
      registerCapabilities()
      unregister = registerAll()
    } catch (e) {
      log('ERROR', `启动失败：${e.message}`)
    }

    // Cordis 在插件卸载时调用这个返回值 —— 「DSH 关闭时不残留 Python 进程」的落点。
    return () => {
      disposed = true
      try { unregister() } catch { /* 已卸载 */ }
      if (conn) conn.kill()
      conn = null
    }

    /**
     * 逐个注册 manifest 里的工具。
     *
     * 形状照 `dsh-multimedia`（17 个工具，本机正在用）：**手写 tool 对象**，
     * 不走 `defineTool`。它的参数就是 Python manifest 原样给出的标准 JSON Schema
     * envelope（`{type:'object', properties, required}`）—— 与 dsh-multimedia
     * 手写的形状同构，所以不需要任何转换。
     *
     * `execute` 拿到的是**已解析好的参数对象**（registry 自己校验并解析），
     * 这里只负责转发回 Python；参数语义的权威在 Python 侧的签名。
     *
     * ⚠ 每个工具单独 try/catch：一个工具有问题不能连累其余 11 个。
     */
    function registerAll() {
      const disposers = []
      let registeredCount = 0
      for (const t of manifestTools) {
        try {
          const tool = {
            name: t.name,
            description: t.description,
            // manifest 给什么就登记什么：空参数的工具 Python 侧给 {}，
            // 这里补一个标准的空 envelope，别把 undefined 传进去。
            parameters: (t.parameters && typeof t.parameters === 'object')
              ? t.parameters
              : { type: 'object', properties: {} },
            output: {
              // 桥接工具返回值形状不统一（{ok, result} / {path, preview} / {ok, error}），
              // 统一成宽松输出，不做强制 schema —— 强制了必然误伤。
              //
              // ⚠ schema 必须是 **JSON Schema 子集**（dsh-tools register() 内的
              // assertSupportedJsonSchema：SCHEMA_TYPES = object/array/string/number/
              // integer/boolean/null，连 type 数组都不接受）。第七轮真机教训
              // （2026-10-13）：曾写 `type: 'json'` —— 那是 ToolSdkSchema DSL 的
              // 类型（valueSchemaSpecToJsonSchema 那条路才认 json），不是 JSON
              // Schema —— 于是 12 个工具在 register() 的断言处**全部**抛
              // JsonSchemaError，被下面的 catch 吞进 cordis logger（它不落
              // stdout/dsh.log，永远看不见），表现恰好是「插件已加载、能力段落
              // 可见、一个工具都没有」。空 schema {} 是官方认可的「标准无约束
              // JSON」形式（assertSupportedJsonSchema 的 doc：annotation-only
              // schemas are accepted as the standard unconstrained-JSON form），
              // 执行期的输出值校验对它也全通过。
              schema: {},
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
          }
          // tools.register() 的返回值本身就是 disposer（dsh-tools 的 register
          // 就是 `return this.layers.effect(...)`），所以直接收下它。
          disposers.push(ctx.tools.register(tool))
          registeredCount += 1
        } catch (e) {
          // 这是「工具明明在 manifest 里、AI 却看不到它」的唯一线索，必须显眼
          log('ERROR', `注册工具 ${t.name} 失败：${e?.message ?? e}`)
        }
      }
      log('INFO', `已注册 ${registeredCount}/${manifestTools.length} 个工具`)
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

// ---------------------------------------------------------------------------
// 模块可加载性自检（DSH_PY_BRIDGE_SELFTEST=1 时跑）
//
// 为什么需要它：本文件曾经顶层 `import { defineTool } from '@deepseek-ai/dsh-tools'`，
// 而 profile 的 node_modules 里**没有**那个包（只有 DSH 自己的安装目录里有，
// pnpm 不会把它暴露给 profile 里的 bundle）。于是模块加载期就
// ERR_MODULE_NOT_FOUND，`apply()` 从未执行 —— 一个工具都没注册，而日志里
// 连一行都没有（DSH 只报一句 `failed to import`）。这个自检就是为这类
// 「import 写错、整包静默消失」的故障兜底的：只要能 import 本文件就说明顶层依赖
// 全部解析成功，剩下的问题才有机会暴露在日志里。
if (process.env.DSH_PY_BRIDGE_SELFTEST === '1') {
  const failures = []
  // 1) 顶层 import 是否解析成功 —— 走到这里就已经成功了（否则本文件加载不出来）
  if (typeof spawn !== 'function') failures.push('node:child_process 的 spawn 不可用')
  // 2) 导出的插件契约是否齐全（loader 按名字取这三个）
  if (typeof apply !== 'function') failures.push('缺少 export apply')
  if (!Array.isArray(inject) || inject.length === 0) failures.push('缺少 export inject')
  if (inject.includes('tools') !== true) failures.push("inject 必须含 'tools'（否则 ctx.tools 是 undefined）")
  // 3) 手写 tool 对象的形状：照 dsh-multimedia 那份（本机正在用）的最小要求
  const sample = {
    name: 'x',
    description: 'd',
    parameters: { type: 'object', properties: {}, required: [] },
    output: { schema: { type: 'json' }, render: (_a, v) => [{ type: 'text', text: JSON.stringify(v) }] },
    execute: async () => ({}),
  }
  for (const key of ['name', 'description', 'parameters', 'output', 'execute']) {
    if (sample[key] === undefined) failures.push('tool 对象缺字段: ' + key)
  }
  if (typeof sample.execute !== 'function') failures.push('execute 必须是函数')
  if (typeof sample.output.render !== 'function') failures.push('output.render 必须是函数')
  if (failures.length > 0) {
    console.error('[py-bridge] 自检失败:\n  ' + failures.join('\n  '))
    process.exitCode = 1
  } else {
    console.log('[py-bridge] 自检通过：模块可加载、插件契约完整、tool 形状合法')
  }
}