# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""markitdown 工具：任意办公/文档格式 → Markdown。

只用 `MarkItDown().convert_local(...)`（**最窄的转换入口**）：显式走本地文件路径，
不接受 URL —— 见 utils.resolve_input_path 的说明（SSRF / 远程内容注入）。
`markitdown[all]` 里的 Azure / 音频转写 extras 需要额外凭据或外部程序（ffmpeg），
这里一律**优雅降级**：缺依赖时返回一句人话，而不是让整个工具组崩掉。

**为什么 xlsx 不走 markitdown**（第十二轮，2026-10-14 真机定案）：
`import markitdown` 会**无条件**拉进全部 18 个 converter（`converters/__init__.py`
顶层逐个 import），其中 `_xlsx_converter` 顶层 `import pandas` → pandas → **numpy 的
C 扩展**。本机 numpy 的 `create_module`（DLL 加载）每进程首次要挂 ~610 秒
（faulthandler 全线程栈实证：GIL 已释放、单线程、无并发、无异常），而转一个 docx
**根本不需要 pandas/numpy** —— 实证：pandas/numpy 缺席时 docx 转换完全正常。

所以这里在 import markitdown 期间把 pandas **临时藏起来**（见 _pandas_hidden）：
`_xlsx_converter` 的 `try/except ImportError` 会干净地接住，docx/pdf/pptx/html/csv
等全部不受影响，numpy 永不被加载；xlsx/xlsm 改由 openpyxl **原生转换**
（见 _xlsx_to_markdown，openpyxl 本来就在基本安装清单里、也已经是 read_excel_data
的底座）—— 能力不减，且完全绕开 pandas。
"""

from __future__ import annotations

import sys
import threading
import types
from contextlib import contextmanager
from pathlib import Path

import dsh_bridge

from utils import (
    DEFAULT_MAX_CHARS,
    ToolError,
    bridge_home,
    diag,
    import_without_numpy,
    numpy_hidden,
    resolve_input_path,
    run_tool,
    truncate_or_dump,
)


def _install_magika_stub() -> None:
    """在 import markitdown **之前**装好 magika 占位。

    markitdown[all] 依赖 magika 做文件类型嗅探，而 magika → onnxruntime 在
    Python 3.14（Windows 构建）的**导入期**就触发原生 access violation
    （0xC0000005，进程级崩溃，try/except 拦不住 —— 真机实测 2026-10-11）。
    唯一安全的拦截点是 sys.modules：先放一个占位模块，让 markitdown 顶层的
    `import magika` 命中缓存而根本不执行真实模块。

    markitdown 0.1.8 对 magika 的调用面只有三处，且**仅当 `result.status == "ok"`**
    才读预测结果，否则降级为「扩展名 + mimetypes」—— stub 永远返回非 "ok" 即可，
    docx/pdf/pptx 等转换不受影响。
    """
    if "magika" in sys.modules:
        return
    module = types.ModuleType("magika")

    class _Output:
        label = "unknown"
        is_text = False
        extensions: tuple = ()
        mime_type = None

    class _Prediction:
        output = _Output()

    class _Result:
        status = "unsupported"  # != "ok" → markitdown 走扩展名降级分支
        prediction = _Prediction()

    class Magika:
        def identify_stream(self, *args, **kwargs):
            return _Result()

        def identify_path(self, *args, **kwargs):  # 0.1.8 未用到，防御
            return _Result()

    module.Magika = Magika
    sys.modules["magika"] = module


_install_magika_stub()

_CONVERTER = None
_CONVERTER_ERROR: str | None = None
# 同时只允许一个线程做「藏 pandas + import markitdown」这套动作：
# 桥接的工具调用跑在线程池里，两个并发 convert 撞在一起会互相看到半个状态。
_IMPORT_LOCK = threading.Lock()
# 超过这个秒数还没 import 完，就往 bridge-diag.log dump 全线程栈（真机排障用）
_IMPORT_WATCHDOG_SECONDS = 60


def _converter():
    """惰性构造 MarkItDown（进程内只造一个，跨调用复用）。

    延迟到第一次真正调用才 import：桥接进程启动时就要把 manifest 交出去，
    万一 markitdown 没装好，不该让**整个桥接**起不来（其它工具仍然可用）。
    """
    global _CONVERTER, _CONVERTER_ERROR
    if _CONVERTER is not None or _CONVERTER_ERROR is not None:
        return _CONVERTER
    with _IMPORT_LOCK:
        if _CONVERTER is not None or _CONVERTER_ERROR is not None:
            return _CONVERTER
        import faulthandler
        import time as _time

        _done = threading.Event()

        def _import_watchdog():
            """import 迟迟不返回时 dump 全线程栈。

            真机教训（2026-10-14）：cordis logger 收不到 Python 侧输出，
            桥接侧卡死时宿主日志一片空白；而这条 dump 一次就定位到了
            numpy 的 create_module。留着它，下一次卡死仍然是可诊断的。
            """
            if _done.is_set():
                return
            try:
                with (bridge_home() / "bridge-diag.log").open("a", encoding="utf-8") as fh:
                    fh.write(
                        f"===== import watchdog @ {_time.strftime('%H:%M:%S')}"
                        "（import markitdown 仍未返回，dump 全线程栈）=====\n"
                    )
                    faulthandler.dump_traceback(file=fh)
                    fh.write("===== end of dump =====\n")
            except Exception:  # noqa: BLE001 — 诊断写不进去绝不能影响工具
                pass
            threading.Timer(120, _import_watchdog).start()

        threading.Timer(_IMPORT_WATCHDOG_SECONDS, _import_watchdog).start()
        try:
            diag("markitdown.import.begin")
            with numpy_hidden() as blocked:
                diag("markitdown.import.numpy-hidden", ",".join(blocked) or "（无）")
                from markitdown import MarkItDown
        except Exception as exc:  # noqa: BLE001
            diag("markitdown.fail", f"{type(exc).__name__}: {str(exc)[:200]}")
            _CONVERTER_ERROR = (
                f"markitdown 不可用：{type(exc).__name__}: {exc}。"
                "请在首选项「Python 环境」里重新点一次「基本安装」。"
            )
            return None
        finally:
            _done.set()
        diag("markitdown.import.done")
        try:
            _CONVERTER = MarkItDown()
        except Exception as exc:  # noqa: BLE001
            diag("markitdown.fail", f"构造失败 {type(exc).__name__}: {str(exc)[:200]}")
            _CONVERTER_ERROR = f"markitdown 构造失败：{type(exc).__name__}: {exc}。"
            return None
        diag("markitdown.construct.done")
    return _CONVERTER


# ---------- xlsx：openpyxl 原生转换（不经 markitdown，见模块文档） ----------

_XLSX_SUFFIXES = frozenset({".xlsx", ".xlsm"})


def _md_table(rows: list[list[str]]) -> str:
    """把二维单元格转成 Markdown 表格（首行当表头）。

    单元格里的 `|` 必须转义，否则整张表会被解析歪；换行压成空格
    （Markdown 表格的单元格里不能有裸换行）。
    """
    if not rows:
        return ""
    width = max(len(r) for r in rows)
    norm = [list(r) + [""] * (width - len(r)) for r in rows]

    def cell(value: str) -> str:
        return value.replace("|", "\\|").replace("\r", " ").replace("\n", " ").strip()

    lines = ["| " + " | ".join(cell(c) for c in norm[0]) + " |"]
    lines.append("| " + " | ".join("---" for _ in range(width)) + " |")
    for row in norm[1:]:
        lines.append("| " + " | ".join(cell(c) for c in row) + " |")
    return "\n".join(lines)


def _xlsx_to_markdown(src: Path) -> str:
    """用 openpyxl 把工作簿转成 Markdown（每个非空工作表一节 + 一张表）。

    刻意不经 markitdown：它的 xlsx 路径要 pandas，而 pandas 会拖入 numpy 的
    C 扩展（本机首次加载挂 ~610s）。openpyxl 是基本安装的必装项、也是
    read_excel_data 的底座，这条路零额外依赖、纯 Python 跑完。
    """
    try:
        # 屏蔽 numpy 后 import：openpyxl 顶层 `import numpy`（带 try/except 保护），
        # 不挡的话这台机器上首次加载要挂 ~610 秒（见 utils.numpy_hidden）
        load_workbook = getattr(import_without_numpy("openpyxl"), "load_workbook", None)
        if load_workbook is None:
            raise ToolError("openpyxl 缺少 load_workbook；请重新执行「基本安装」。")
    except ImportError as exc:  # noqa: BLE001
        raise ToolError(f"openpyxl 不可用：{exc}。请重新执行「基本安装」。") from exc

    try:
        # read_only：只读流式，几万行的表也不会把内存吃满
        # data_only：取公式的**计算结果**而不是 "=SUM(A1:A9)" 本身
        wb = load_workbook(str(src), read_only=True, data_only=True)
    except Exception as exc:  # noqa: BLE001 — 加密 / 损坏 / 其实不是 xlsx
        raise ToolError(f"打开 {src.name} 失败：{type(exc).__name__}: {exc}") from exc

    parts: list[str] = []
    try:
        for ws in wb.worksheets:
            rows: list[list[str]] = []
            for row in ws.iter_rows(values_only=True):
                cells = ["" if c is None else str(c) for c in row]
                if any(c.strip() for c in cells):  # 整行空白就丢掉
                    rows.append(cells)
            if not rows:
                continue
            parts.append(f"## {ws.title}")
            parts.append("")
            parts.append(_md_table(rows))
            parts.append("")
    finally:
        try:
            wb.close()
        except Exception:  # noqa: BLE001
            pass

    if not parts:
        raise ToolError(f"{src.name} 里没有可提取的内容（所有工作表都是空的）。")
    return "\n".join(parts).strip()


@dsh_bridge.tool(
    name="convert_file_to_markdown",
    description=(
        "把一个本机文档（docx / pptx / xlsx / xlsm / pdf / html / csv / json / xml / "
        "epub / Outlook 邮件等）转成 Markdown 文本。当用户给了文档让你「读一下 / "
        "总结 / 提取内容」，或需要把非 Markdown 的资料喂进后续处理时用它。"
        "只接受本机文件绝对路径，不接受 URL。"
        "返回：正文很小时直接给全文；超过 max_chars 时改为返回 "
        "{path, chars, lines, preview}，全文已写入 path 指向的 .md 文件。"
        "如果转换成功但 extras 缺失（如音频转写需要 ffmpeg），返回 ok=false 并说明原因。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "要转换的本机文件绝对路径（不接受 URL）",
            },
            "output_path": {
                "type": "string",
                "description": "可选：把 Markdown 直接写到这个 .md 路径；留空则由工具决定（超长时落盘）",
            },
            "max_chars": {
                "type": "integer",
                "description": f"正文长度上限，超过则落盘只回预览；默认 {DEFAULT_MAX_CHARS}",
                "minimum": 0,
            },
        },
        "required": ["file_path"],
    },
)
def convert_file_to_markdown(
    file_path: str,
    output_path: str | None = None,
    max_chars: int = DEFAULT_MAX_CHARS,
) -> dict:
    def work():
        src = resolve_input_path(file_path)
        diag("convert.begin", f"{src} ({src.stat().st_size} bytes)")
        suffix = src.suffix.lower()

        if suffix in _XLSX_SUFFIXES:
            text = _xlsx_to_markdown(src)
            diag("convert.xlsx.done", f"{len(text)} chars")
        elif suffix == ".xls":
            raise ToolError(
                "旧版 .xls 不受支持：读它要 pandas+xlrd，而 pandas 会拖入在本机"
                "会挂死的 numpy C 扩展。请用 Excel 另存为 .xlsx 后重试。"
            )
        else:
            md = _converter()
            if md is None:
                raise ToolError(_CONVERTER_ERROR or "markitdown 不可用。")
            try:
                result = md.convert_local(str(src))
            except Exception as exc:  # noqa: BLE001 — 格式不支持 / extras 缺失 / 文件损坏
                raise ToolError(
                    f"转换失败：{type(exc).__name__}: {exc}"
                    "（音频转写需要 ffmpeg、Azure 相关转换需要凭据；纯文档格式不受影响。）"
                ) from exc
            text = getattr(result, "markdown", None)
            if text is None:
                raise ToolError("markitdown 没有返回内容（可能是不支持的格式）。")
            text = str(text)
            diag("convert.markitdown.done", f"{len(text)} chars")

        if output_path and str(output_path).strip():
            dest = resolve_input_path(output_path, must_exist=False)
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(text, encoding="utf-8")
            return {
                "source": str(src),
                "path": str(dest),
                "chars": len(text),
                "lines": text.count("\n") + 1 if text else 0,
                "truncated": False,
            }
        dumped = truncate_or_dump(text, max_chars, name_hint=src.stem or "converted")
        return {"source": str(src), **dumped}

    return run_tool(work)


@dsh_bridge.tool(
    name="convert_files_to_markdown",
    description=(
        "批量把多个本机文档转成 Markdown。适合「把这个文件夹里的报告都整理成 md」"
        "这类一次性任务。单个文件失败**不会**中断整批：每条结果独立给出 ok / error，"
        "返回 {results: [...], ok_count, fail_count}。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "file_paths": {
                "type": "array",
                "items": {"type": "string"},
                "description": "本机文件绝对路径列表（不接受 URL）",
            },
            "output_dir": {
                "type": "string",
                "description": "可选：把所有 Markdown 写到这个目录（同名 .md）；留空则每条各自判断是否落盘",
            },
        },
        "required": ["file_paths"],
    },
)
def convert_files_to_markdown(file_paths: list[str], output_dir: str | None = None) -> dict:
    def work():
        if not isinstance(file_paths, (list, tuple)) or not file_paths:
            raise ToolError("file_paths 不能为空；请给一个文件路径数组。")
        if len(file_paths) > 50:
            raise ToolError(f"一次最多处理 50 个文件（收到 {len(file_paths)} 个），请分批。")
        out_dir = None
        if output_dir and str(output_dir).strip():
            out_dir = resolve_input_path(output_dir, must_exist=False)
            out_dir.mkdir(parents=True, exist_ok=True)

        results = []
        for raw in file_paths:
            try:
                single = convert_file_to_markdown(
                    raw,
                    output_path=str(out_dir / (Path(raw).stem + ".md")) if out_dir else None,
                    max_chars=DEFAULT_MAX_CHARS,
                )
                results.append({"file_path": raw, **single})
            except Exception as exc:  # noqa: BLE001 — 一条坏文件不许毁掉整批
                results.append(
                    {"file_path": raw, "ok": False, "error": f"{type(exc).__name__}: {exc}"}
                )
        ok_count = sum(1 for r in results if r.get("ok"))
        return {
            "results": results,
            "ok_count": ok_count,
            "fail_count": len(results) - ok_count,
        }

    return run_tool(work)


@dsh_bridge.tool(
    name="markitdown_capabilities",
    description=(
        "报告当前环境里文档转换的可用路径与已知限制。"
        "在批量转换失败、或用户问「为什么这个格式转不了」时先调它，"
        "拿到的是一份明确的清单，而不是猜测。"
    ),
    parameters={"type": "object", "properties": {}},
)
def markitdown_capabilities() -> dict:
    def work():
        # 纯静态报告：不 import 任何东西。它曾经是 ~600 秒挂死的受害者
        # （那时它自己触发 import markitdown / 扫 dist-info），现在只回答事实。
        info: dict = {
            "mode": "in-process",
            "routes": {
                "xlsx/xlsm": "openpyxl 原生转换（不经 markitdown，不需要 pandas）",
                "docx": "markitdown（mammoth）",
                "pptx": "markitdown（python-pptx）",
                "pdf": "markitdown（pdfminer.six + pdfplumber）",
                "html/csv/json/xml/epub/Outlook 邮件/zip": "markitdown",
                "xls": "不支持（旧版格式，读它要 pandas+xlrd；请另存为 .xlsx）",
            },
            "note": (
                "转换在桥接常驻进程内完成。import markitdown 时会临时屏蔽 pandas，"
                "以免它拖入 numpy 的 C 扩展（本机每进程首次加载会挂约 10 分钟）；"
                "docx/pdf/pptx 等格式本就不需要 pandas，xlsx 由 openpyxl 原生处理。"
            ),
        }
        if _CONVERTER_ERROR:
            info["error"] = _CONVERTER_ERROR
        return info

    return run_tool(work)
