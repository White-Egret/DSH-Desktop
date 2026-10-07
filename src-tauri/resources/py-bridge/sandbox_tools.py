# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""通用 Python 沙箱：给预定义工具覆盖不到的需求一条出路。

边界（刻意保守，且**双层**）：
  ① 导入白名单：替换 `__builtins__.__import__`，只有 ALLOWED_MODULES 里的顶层模块放行；
  ② 受限 builtins：`eval / exec / compile / __import__ / globals / locals / getattr(下划线)`
     全部拿掉 —— 只挡导入不挡 eval 等于没挡（`eval("__import__('os')...")` 一行就绕过去了）。

为什么连 `os` 都不给：DSH 自带 pwsh / bash / 文件类工具，这里要做的是
「用这些**库**做点算事」，不是给 AI 再开一个逃逸口。白名单里的数据科学库
（pandas/numpy/…）是「装了即用」：没装时 import 抛的是普通 ImportError，
读起来比「被策略拒绝」自然得多。
"""

from __future__ import annotations

import builtins as _real_builtins

import dsh_bridge

from utils import DEFAULT_MAX_CHARS, jsonable, run_tool

# ---- 导入白名单 ------------------------------------------------------------
#
# 三类：
#   1. 常用标准库（纯计算，不碰系统）；
#   2. 基本安装带的办公库 + markitdown；
#   3. 数据分析库 —— **预置但不强求安装**：用户日后装了 pandas 即可自动可用，
#      没装时是普通的 ModuleNotFoundError，而不是一句「被沙箱拒绝」。
ALLOWED_MODULES: frozenset[str] = frozenset(
    {
        # —— 标准库：纯计算 / 格式化 ——
        "json", "math", "statistics", "re", "datetime", "time", "random",
        "itertools", "functools", "collections", "string", "textwrap",
        "decimal", "fractions", "numbers", "unicodedata", "hashlib",
        "base64", "binascii", "csv", "copy", "heapq", "bisect", "uuid",
        "dataclasses", "enum", "typing", "abc", "operator", "array",
        "struct", "zlib", "gzip", "difflib", "pprint", "types",
        # —— 办公文档（基本安装） ——
        "markitdown", "docx", "pptx", "openpyxl", "xlsxwriter", "lxml",
        "PIL", "et_xmlfile", "typing_extensions", "xlrd", "xlwt", "odf",
        # —— 数据分析（装了即用） ——
        "numpy", "pandas", "scipy", "matplotlib", "sklearn",
        "statsmodels", "seaborn", "plotly", "sympy",
    }
)

# 这些一律 ImportError，不看白名单：它们是逃逸口，不是「危险但偶尔有用」的库
BLOCKED_MODULES: frozenset[str] = frozenset(
    {
        "os", "sys", "subprocess", "socket", "shutil", "ctypes", "importlib",
        "pickle", "marshal", "shelve", "multiprocessing", "threading",
        "pty", "signal", "resource", "glob", "tempfile", "pathlib", "io",
        "webbrowser", "urllib", "http", "requests", "ftplib", "smtplib",
        "asyncio", "atexit", "gc", "inspect", "runpy", "builtins",
        "code", "codeop", "pdb", "bdb", "trace", "doctest",
    }
)

# 从标准库里被移除 / 本来就危险的 builtins 名字。
# `__import__` 单独留着自己实现（做白名单），但**不暴露给用户代码**。
UNSAFE_BUILTINS: frozenset[str] = frozenset(
    {
        "eval", "exec", "compile", "execfile", "__import__",
        "globals", "vars", "locals", "input", "breakpoint", "exit", "quit",
        "help", "memoryview", "object", "super", "type",
    }
)

# `result` 是约定的返回值变量名（见工具 description）
RESULT_VAR = "result"


class SandboxImportBlocked(ImportError):
    """**策略**拒绝：模块在黑名单里，或不在白名单里。

    单独建类是为了和「这个库压根没装」区分开 —— 两者都是 ImportError，
    但只有前者值得告诉模型「换个写法」，后者只需要说「先装它」。
    """


def _guard_import(name, globals=None, locals=None, fromlist=(), level=0):  # noqa: A002
    """替换 `__import__`：先看黑名单，再看白名单。

    `name` 是**顶层**模块名（`import os.path` 也会传 `os`），所以只看第一段 ——
    放行 `xml` 却让 `xml.sax` 里的东西进来并不安全，但这里本来就按顶层粒度放行。
    """
    root = (name or "").split(".")[0]
    if not root:
        raise SandboxImportBlocked(f"沙箱：空模块名（{name!r}）")
    if root in BLOCKED_MODULES or root == "builtins":
        raise SandboxImportBlocked(
            f"沙箱禁止导入 {root}：它可用于访问系统或起子进程。"
            "本沙箱只放行纯计算与文档/数据处理库；需要执行外部命令请用 DSH 自带的 shell 工具。"
        )
    if root not in ALLOWED_MODULES:
        raise SandboxImportBlocked(
            f"沙箱未放行模块 {root}。可用模块："
            + "、".join(sorted(ALLOWED_MODULES))
            + "（数据类库需先在首选项「Python 环境」里安装才会生效）"
        )
    return _real_import(name, globals, locals, fromlist, level)


_real_import = _real_builtins.__import__


def make_restricted_builtins() -> dict:
    """构造受限 builtins 字典（含我们自己的白名单 __import__）。"""
    safe = {
        k: v
        for k, v in _real_builtins.__dict__.items()
        if not k.startswith("__") and k not in UNSAFE_BUILTINS
    }
    # `getattr` 能取到 `__class__` → `__subclasses__` → 全局逃逸，所以挡掉下划线属性
    _get = _real_builtins.getattr

    def guarded_getattr(obj, name, *a):
        if isinstance(name, str) and name.startswith("_"):
            raise AttributeError(f"沙箱禁止访问下划线属性 {name!r}")
        return _get(obj, name, *a)

    def guarded_setattr(obj, name, value):
        if isinstance(name, str) and name.startswith("__"):
            raise AttributeError(f"沙箱禁止修改 {name!r}")
        return _real_builtins.setattr(obj, name, value)

    safe["getattr"] = guarded_getattr
    safe["setattr"] = guarded_setattr
    safe["__import__"] = _guard_import
    return safe


@dsh_bridge.tool(
    name="execute_python_sandbox",
    description=(
        "在受限沙箱里执行一段 Python 代码，用来处理上面那些专用工具没覆盖到的需求"
        "（批量统计、复杂格式化、跨库的数据整理）。"
        "**把结果赋给变量 `result`**（可以是 dict / list / 数字 / 字符串）；"
        "没有 `result` 时返回 stdout 捕获。"
        "可用库：markitdown、docx、pptx、openpyxl、xlsxwriter、PIL、lxml 与常用标准库；"
        "装了 pandas / numpy / scipy / matplotlib / sklearn 也会自动放行。"
        "**禁止** os / sys / subprocess / socket / shutil / ctypes / importlib / 路径读写 "
        "以及 eval/exec/compile/open —— 它们一律 ImportError。"
        "结果会自动截断：超长时只回预览（DataFrame 取 head + 摘要）。"
        "异常会变成 {ok: false, error}，不会让桥接进程退出。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "code": {"type": "string", "description": "要执行的 Python 代码；结果请赋给 result"},
        },
        "required": ["code"],
    },
)
def execute_python_sandbox(code: str) -> dict:
    def work():
        if not isinstance(code, str) or not code.strip():
            raise ValueError("code 不能为空。")
        if len(code) > 200_000:
            raise ValueError("代码过长（上限 200000 字符）。")

        # 每次调用一份全新的命名空间：上一次留下的变量不该泄漏到这一次
        safe_builtins = make_restricted_builtins()
        sandbox_globals: dict = {
            "__builtins__": safe_builtins,
            "__name__": "__dsh_sandbox__",
        }

        # 用户代码里的 print 不会污染 stdout（runtime 把 stdout 当协议通道），
        # 这里把 print 重定向到 StringIO，回给宿主当「输出」。
        import io
        import contextlib

        buf = io.StringIO()
        try:
            with contextlib.redirect_stdout(buf):
                compiled = compile(code, "<dsh-sandbox>", "exec")
                exec(compiled, sandbox_globals)  # noqa: S102 — 这就是沙箱要做的事
        except SandboxImportBlocked as exc:
            # 明确是**策略**拒绝：模型该换写法，而不是换个装法
            return {"ok": False, "error": f"导入被沙箱拦截：{exc}", "kind": "sandbox-blocked"}
        except ImportError as exc:
            # 模块在白名单里、只是这台机器没装（pandas 之类）：说清该怎么补
            return {
                "ok": False,
                "error": f"导入失败：{exc}（该模块在沙箱白名单内，但当前 Python 环境里没装；"
                "可到 DSH 首选项「Python 环境」里安装后重试）",
                "kind": "sandbox-missing-module",
            }
        except Exception as exc:  # noqa: BLE001 — 用户的代码出错是他的事，不是桥接的事
            return {
                "ok": False,
                "error": f"{type(exc).__name__}: {exc}",
                "kind": "sandbox-exception",
                "stdout": buf.getvalue()[:DEFAULT_MAX_CHARS],
            }

        out: dict = {"ok": True}
        captured = buf.getvalue()
        if captured.strip():
            out["stdout"] = captured[:DEFAULT_MAX_CHARS]

        if RESULT_VAR in sandbox_globals:
            value = sandbox_globals[RESULT_VAR]
            # DataFrame 之类：走 head(N)+摘要，而不是整表转 JSON
            shape = getattr(value, "shape", None)
            if shape is not None and hasattr(value, "head"):
                try:
                    out["shape"] = list(shape)
                    out["columns"] = [str(c) for c in getattr(value, "columns", [])][:100]
                    out["head"] = jsonable(value.head(20).to_dict(), max_chars=DEFAULT_MAX_CHARS)
                    out["note"] = "结果是一个表：已只返回前 20 行与列名，需要更多请自行切片。"
                except Exception as exc:  # noqa: BLE001
                    out["result"] = jsonable(value, max_chars=DEFAULT_MAX_CHARS)
                    out["note"] = f"表摘要失败（{exc}），已退回普通序列化。"
            else:
                out["result"] = jsonable(value, max_chars=DEFAULT_MAX_CHARS)
        else:
            out["result"] = None
            out["note"] = f"代码没有给 {RESULT_VAR} 赋值；这是有意为之吗？"
        return out

    return run_tool(work)


def sandbox_module_list() -> list[str]:
    """给 env_tools 组提示词用：当前放行的模块清单。"""
    return sorted(ALLOWED_MODULES)