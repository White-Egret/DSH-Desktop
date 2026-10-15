# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）
# 由 src-tauri/src/py_bridge.rs 的 deploy_release 写入；改动请改 Rust 侧的常量后重装。
#
# 单进程多工具：dsh_bridge.runtime 只 spawn 这一个模块，所有 @tool 装饰器
# 在 import 时一次性注册进同一个 registry，再统一走 NDJSON JSON-RPC。
#
# 启动方式：<python> -u -m dsh_bridge.runtime bridge_entry
# （cwd = 本目录；-u 必须加，否则管道里 Python 会块缓冲，stdout 不是协议流）

"""共用工具：路径校验、结果序列化、输出截断。

这里是**唯一的**「工具公共层」：路径白名单与截断规则如果散落在各工具文件里，
迟早会有某一个工具绕过它们 —— 所以刻意集中在这里，且被每个工具模块 import。
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import time
from contextlib import contextmanager
from pathlib import Path

# stdio 上跑 JSON-RPC：几 MB 的正文会挤爆宿主侧的工具结果缓存，
# 也会把模型上下文撑成一个不可用的会话。超过这个长度一律落盘 + 回摘要。
DEFAULT_MAX_CHARS = 20_000
# 单个返回值序列化后的硬上限（超过就报错而不是继续膨胀）
HARD_MAX_CHARS = 2_000_000


class ToolError(Exception):
    """工具内部的可预期错误。

    单独建一类而不是直接用 ValueError：runtime 会把 ValueError 映射成
    -32004/invalid-args，对「文件不存在」「extras 没装」这类**外部条件**
    并不准确；用它能让错误语义与真正用错的参数区分开。
    """


def bridge_home() -> Path:
    """桥接产物目录（写文件、落盘长结果的默认位置）。

    由宿主通过环境变量 DSH_PY_BRIDGE_HOME 传入；取不到就退回用户临时目录 ——
    宁可让结果落在临时目录，也不能因为取不到路径就让整个工具组不可用。
    """
    raw = os.environ.get("DSH_PY_BRIDGE_HOME", "").strip()
    if raw:
        return Path(raw)
    return Path(tempfile.gettempdir()) / "dsh-desktop-py-bridge"


def resolve_input_path(raw: str, *, must_exist: bool = True) -> Path:
    """校验一个**输入**文件路径。

    刻意不接受 URL：MarkItDown 默认能转 http(s) URL，那意味着工具入口可以
    被诱导去请求任意地址（SSRF / 内网探测 / 远程内容注入）。工具只认本地路径。
    """
    if not isinstance(raw, str) or not raw.strip():
        raise ToolError("file_path 不能为空；请给出本机文件的绝对路径。")
    text = raw.strip()
    if text.lower().startswith(("http://", "https://", "ftp://", "file://")):
        raise ToolError(
            "只接受本机文件路径，不接受 URL（远程地址会带来 SSRF 与远程内容注入风险）。"
        )
    p = Path(text).expanduser()
    try:
        p = p.resolve(strict=False)
    except OSError as exc:  # 盘符不存在、权限异常等
        raise ToolError(f"无法解析路径 {text}：{exc}") from exc
    if must_exist and not p.is_file():
        raise ToolError(f"文件不存在：{p}")
    return p


def truncate_or_dump(
    text: str,
    max_chars: int = DEFAULT_MAX_CHARS,
    *,
    name_hint: str = "dsh_python_output",
    output_dir: Path | None = None,
) -> dict:
    """超长正文落盘、短正文直返。

    返回 `{path?, chars, lines, preview, truncated}`；调用方按同一形状返回给宿主。
    落盘目录优先取 output_dir，其次是桥接 home —— **绝不写进用户随手给的目录**。
    """
    if max_chars <= 0:
        max_chars = DEFAULT_MAX_CHARS
    chars = len(text)
    lines = text.count("\n") + 1 if text else 0
    if chars <= max_chars:
        return {
            "chars": chars,
            "lines": lines,
            "preview": text,
            "truncated": False,
        }
    base = output_dir or bridge_home()
    try:
        base.mkdir(parents=True, exist_ok=True)
        fd, name = tempfile.mkstemp(prefix=f"{name_hint}_", suffix=".md", dir=str(base))
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(text)
    except OSError as exc:
        # 落盘失败不能反过来让整个转换失败：退回「只回预览」，并如实说明没有全文文件
        return {
            "chars": chars,
            "lines": lines,
            "preview": text[:max_chars],
            "truncated": True,
            "path": None,
            "note": f"全文写入失败（{exc}），这里只返回了前 {max_chars} 个字符。",
        }
    return {
        "chars": chars,
        "lines": lines,
        "preview": text[:max_chars],
        "truncated": True,
        "path": name,
    }


def jsonable(value, *, max_chars: int = DEFAULT_MAX_CHARS) -> dict:
    """把任意 Python 值序列化成「有上限」的 JSON 安全结构。

    DataFrame 走 head(N) + 形状摘要而不是整表转 JSON —— 一张几千行的表
    转成 JSON 既慢又会直接撑爆上下文（这是数据分析沙箱最常见的翻车点）。

    返回**不带** `ok` 字段（外层调用方已经加过了）：这里只回答「值变成了什么」，
    嵌套一个同名字段会让调用方收到 `result.result.ok` 这种没人想读的三层结构。
    """
    try:
        text = json.dumps(value, ensure_ascii=False, default=str)
    except (TypeError, ValueError) as exc:
        return {"error": f"结果无法序列化为 JSON：{exc}", "repr": repr(value)[:2000]}
    if len(text) <= max_chars:
        return {"value": value, "chars": len(text)}
    return {
        "truncated": True,
        "chars": len(text),
        "preview": text[:max_chars],
        "note": f"完整结果共 {len(text)} 个字符，已截断；需要细节请缩小范围或只取 head(N)。",
    }


def run_tool(func, *args, **kwargs) -> dict:
    """统一错误收口：任何异常都变成 `{"ok": false, "error": ...}`。

    绝不让异常冒到 runtime：runtime 会把未捕获异常变成 JSON-RPC error frame，
    而一个 `FileNotFoundError` 应该是「这次调用失败了、可以换个路径重试」，
    不是「工具挂了」。桥接进程**任何情况下都必须活着**（它服务全部工具）。
    """
    # 落盘诊断（第九轮真机教训）：桥接侧的 stderr/stdout 日志被 runtime 收走后
    # 走 cordis logger，宿主日志里永远看不见 —— 转换卡死时无从定位卡在哪一步。
    # 这里把每个工具的入口/出口/异常连同用时写进 bridge-diag.log。
    caller = sys._getframe(1).f_code.co_name if sys._getframe(1) is not None else "?"
    diag("tool.start", caller)
    t0 = time.perf_counter()
    try:
        result = func(*args, **kwargs)
    except ToolError as exc:
        diag("tool.tool-error", f"{caller} | {time.perf_counter() - t0:.2f}s | {str(exc)[:200]}")
        return {"ok": False, "error": str(exc), "kind": "tool-error"}
    except Exception as exc:  # noqa: BLE001 — 收口即目标
        diag("tool.exception", f"{caller} | {time.perf_counter() - t0:.2f}s | {type(exc).__name__}: {str(exc)[:200]}")
        return {
            "ok": False,
            "error": f"{type(exc).__name__}: {exc}",
            "kind": "exception",
        }
    diag("tool.done", f"{caller} | {time.perf_counter() - t0:.2f}s")
    if isinstance(result, dict) and "ok" in result:
        return result
    return {"ok": True, "result": result}


# ---------- numpy 规避（第十二轮真机 + 本机实证，2026-10-14） ----------
#
# 病象：`import numpy` 的 C 扩展（`numpy._core.multiarray` 的 create_module =
# DLL 加载 + PyInit）在**某些 Windows 机器**上每进程首次要挂 ~610 秒才返回
# （faulthandler 全线程栈实证：GIL 已释放、单线程、无并发、无异常 —— 就是等）。
# 而下面这条链会在**每次冷启动**把 numpy 拖进来：
#
#     import openpyxl  → openpyxl/compat/numbers.py 顶层 import numpy
#     import markitdown → converters/_xlsx_converter.py 顶层 import pandas → numpy
#
# 两处**都有 try/except ImportError 保护**（缺了就把 NUMPY / 依赖标记置假，
# 功能降级但不报错），所以「暂时不让它导入」是上游支持的路径，不是 hack。
# 而转 docx / pdf / pptx / html / csv **根本不需要** numpy：
# 本机实证 —— pandas/numpy 缺席时 docx 转换完全正常。

# 用于区分「键不存在」与「键存在但值是 None」
_MISSING = object()

# 「藏 numpy」期间的全局锁：桥接的工具调用跑在线程池里，两个并发调用撞在一起
# 会互相看到半个状态（一个刚放上 None、另一个正要 import 真模块）。
_NUMPY_HIDE_LOCK = threading.RLock()


@contextmanager
def numpy_hidden():
    """在 import openpyxl / markitdown 期间把 numpy（与 pandas）藏起来。

    **为什么必须连 pandas 一起屏蔽**：`_xlsx_converter` 顶层 import pandas，
    而 pandas 顶层 import numpy —— 只挡 numpy 的话 pandas 仍会失败并留下
    半个模块状态；两个一起挡，两处的 try/except 都能干净接住。

    **为什么不能只挡 pandas**（这是第十一轮的错误）：**openpyxl 自己**就顶层
    `import numpy`，而 openpyxl 是 read_excel_data 与 xlsx 转换的底座，
    必然被 import —— 于是 numpy 照样进来，~610 秒照旧。实测对照（本机）：

      · 不屏蔽：import openpyxl → numpy 真模块=True、C 扩展=True
      · 屏蔽后：import openpyxl → numpy 真模块=False、C 扩展=False，
                openpyxl 的 NUMPY 常量干净地变成 False，docx 转换照常成功

    探针提醒：`"numpy" in sys.modules` 是**错的判据** —— 屏蔽期间它返回 True
    （值是 None）。要看 `sys.modules.get("numpy")` 是不是真模块对象，
    或看 `numpy._core*` 这类 C 扩展在不在。

    两种情况**刻意区分**：
      · 还没被导入 → 屏蔽（正是要避免的那条链）；
      · 已经导入过（用户先在沙箱里做过数据分析）→ 说明 numpy 早加载完了，
        这时**不动它**，让 openpyxl / markitdown 拿到真模块。

    退出时只**解除屏蔽**，绝不替用户 import —— 否则沙箱里的 `import numpy`
    会莫名其妙失败，那才是真正的能力回归。嵌套调用由 RLock + 引用计数安全处理。
    """
    with _NUMPY_HIDE_LOCK:
        blocked: list[str] = []
        for name in ("numpy", "pandas"):
            if name not in sys.modules:
                sys.modules[name] = None
                blocked.append(name)
        try:
            yield blocked
        finally:
            for name in blocked:
                # 只删掉我们自己放的那个 None；万一有人在这期间真的导入了，就别动
                if sys.modules.get(name, _MISSING) is None:
                    del sys.modules[name]


def import_without_numpy(module_name: str, **import_kwargs):
    """import 一个模块，期间屏蔽 numpy/pandas（见 numpy_hidden 的说明）。

    给 openpyxl / markitdown 这类「顶层会顺手拖 numpy」的库用。
    已经 import 过的模块直接返回（第二次调用不会重复屏蔽，也就没有开销）。
    """
    if module_name in sys.modules and sys.modules[module_name] is not None:
        return sys.modules[module_name]
    with numpy_hidden():
        return __import__(module_name, **import_kwargs)


# ---------- 落盘诊断 ----------
#
# 桥接进程的 stderr 会被 runtime 转给宿主的 cordis logger，而 cordis logger
# **不落 stdout / dsh.log** —— 真机排障时等于黑洞。这里的日志直接 append 到
# 桥接目录下的 bridge-diag.log，任何编辑器/记事本都能看。失败静默：诊断
# 写不进去绝不能反过来影响工具。

_DIAG_T0 = time.perf_counter()


def diag(stage: str, detail: str = "") -> None:
    """把关键阶段写进 `<桥接目录>\\bridge-diag.log`（带时间戳与进程累计耗时）。"""
    try:
        now = time.perf_counter()
        ts = time.strftime("%H:%M:%S") + f".{int(now % 1 * 1000):03d}"
        line = f"[{ts} +{now - _DIAG_T0:8.2f}s] {stage}" + (f" | {detail}" if detail else "")
        with (bridge_home() / "bridge-diag.log").open("a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    except OSError:
        pass