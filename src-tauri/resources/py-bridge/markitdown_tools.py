# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""markitdown 工具：任意办公/文档格式 → Markdown。

只用 `MarkItDown().convert_local(...)`（**最窄的转换入口**）：显式走本地文件路径，
不接受 URL —— 见 utils.resolve_input_path 的说明（SSRF / 远程内容注入）。
音频转写 / Azure 文档智能这类能力需要额外凭据或外部程序（ffmpeg），
这里一律**优雅降级**：缺依赖时返回一句人话，而不是让整个工具组崩掉。

**依赖边界（2026-10-17 收窄）**：基本安装用裸 `markitdown`，不再用
`markitdown[all]`。所以 PDF / DOCX / Outlook 的后端改为**由基本清单显式安装**：
`pdfminer-six` + `pdfplumber`（PDF）、`mammoth`（DOCX）、`olefile`（.msg）。
这些在 markitdown 里都是**可选导入** —— 缺了它不报错，只在真正转换对应格式时
抛 `MissingDependencyException`，所以状态行（PY_BASIC_MODULES）也单独检查它们。

**为什么 xlsx 不走 markitdown**（第十二轮，2026-10-14 真机定案）：
`import markitdown` 会**无条件**拉进全部 18 个 converter（`converters/__init__.py`
顶层逐个 import），其中 `_xlsx_converter` 顶层 `import pandas` → pandas → **numpy 的
C 扩展**。本机 numpy 的 `create_module`（DLL 加载）每进程首次要挂 ~610 秒
（faulthandler 全线程栈实证：GIL 已释放、单线程、无并发、无异常），而转一个 docx
**根本不需要 pandas/numpy** —— 实证：pandas/numpy 缺席时 docx 转换完全正常。

所以这里在 import markitdown 期间把 numpy/pandas **临时藏起来**（见
utils.numpy_hidden）：`_xlsx_converter` 与 openpyxl 的 `try/except ImportError`
都会干净地接住，docx/pdf/pptx/html/csv 等全部不受影响，numpy 永不被加载；
xlsx/xlsm 改由 openpyxl **原生转换**（见 _xlsx_to_markdown，openpyxl 本来就在
基本安装清单里、也已经是 read_excel_data 的底座）—— 能力不减，且完全绕开 pandas。

**三路路由（2026-10-24 起）**：本模块只负责「本地怎么转」与工具面，**路由决策与
云端编排全部在 `doc_route.py`**。这里抽出 `convert_local_to_markdown` 作为唯一的
本地转换入口，`convert_any` 把它当兜底注入 —— 刻意不做成 doc_route 里 import
本模块，那会形成循环依赖。`prefer` 参数见工具描述。
"""

from __future__ import annotations

import sys
import threading
import types
from contextlib import contextmanager
from pathlib import Path

import dsh_bridge

import doc_route
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

    magika 是 markitdown 的**无条件依赖**（不是 extras —— 裸 `markitdown`
    也照样带它），而 magika → onnxruntime 在 Python 3.14（Windows 构建）的
    **导入期**就触发原生 access violation（0xC0000005，进程级崩溃，
    try/except 拦不住 —— 真机实测 2026-10-11）。
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


def convert_local_to_markdown(src: Path) -> str:
    """**本地**转换的唯一入口：任意本机文档 → Markdown，不联网、不上传。

    三路路由里它是所有路线的最后一级兜底（`doc_route.convert_any` 把本函数注入），
    也是 `prefer="fast"` 与 Office/HTML/CSV 的**唯一**执行者 —— 所以它必须
    自给自足：不依赖 doc_route 的任何状态，也不吞异常（抛 ToolError 由上层判断
    这次失败算不算「还能交付」）。

    xlsx/xlsm 走 openpyxl 原生路径，其余走 markitdown；`.xls` 明确拒绝
    （读它要 pandas+xlrd，而 pandas 会拖入本机会挂死的 numpy C 扩展）。
    """
    suffix = src.suffix.lower()
    if suffix in _XLSX_SUFFIXES:
        text = _xlsx_to_markdown(src)
        diag("convert.xlsx.done", f"{len(text)} chars")
        return text
    if suffix == ".xls":
        raise ToolError(
            "旧版 .xls 不受支持：读它要 pandas+xlrd，而 pandas 会拖入在本机"
            "会挂死的 numpy C 扩展。请用 Excel 另存为 .xlsx 后重试。"
        )
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
    return text


@dsh_bridge.tool(
    name="convert_file_to_markdown",
    description=(
        "把一个本机文档转成 Markdown，自动选引擎（三路路由）："
        "Office（docx/pptx/xlsx）/ HTML / CSV / JSON / XML / epub / Outlook 邮件，"
        "以及**文本层干净、无表格的 PDF** 走本地 markitdown（不联网、0.1 秒级）；"
        "**含表格或复杂版面的 PDF、扫描件 PDF、以及独立图片** 走云端版面模型 OXR"
        "（会把文件上传到 SoMark 免费服务，单次 ≤10MB/≤20 页，无 key、免费）；"
        "**当版面模型不够好时（多音标小语种、手写、极小字号、需要逐字准确引用），"
        "传 prefer=\"accuracy\"：工具会把页面渲染成 PNG 并把绝对路径交给你，"
        "由你**自己用 read_image 读图转写**——这条最准，因为不经任何 OCR，"
        "也不需要任何 API Key。**注意：这条路返回的是图片路径而不是正文**。"
        "云端不可用时自动回落本地并在 degraded/note 里标注，不会返回一份空文件。"
        "双栏/多栏论文、带公式的试卷、发票等版面敏感文档传 prefer=\"layout\"；"
        "离线或只要纯文本传 prefer=\"fast\"。"
        "只接受本机绝对路径，不接受 URL。"
        "返回 {ok, engine, route, source, path, chars, preview}"
        "（engine 是 markitdown / oxr / page-images / markitdown-fallback；"
        "page-images 时另给 page_images 路径数组与 need_agent_vision=true）。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "要转换的本机文件绝对路径（不接受 URL）",
            },
            "prefer": {
                "type": "string",
                "enum": list(doc_route.VALID_PREFER),
                "description": (
                    "引擎偏好，默认 auto（按侦察结果自动选）。"
                    "auto = 自动判；fast = 只用本地（离线/批量/只要纯文本）；"
                    "layout = 强制云端版面模型 OXR（双栏论文、公式试卷、发票）；"
                    "accuracy = 把页面渲染成图交给你自己读（多音标小语种、手写、逐字引用；"
                    "仅对图片与扫描件生效，有干净文本层的 PDF 仍走本地）。"
                ),
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
    prefer: str = "auto",
    output_path: str | None = None,
    max_chars: int = DEFAULT_MAX_CHARS,
) -> dict:
    def work():
        src = resolve_input_path(file_path)
        diag("convert.begin", f"{src} ({src.stat().st_size} bytes) prefer={prefer}")

        outcome = doc_route.convert_any(
            src, prefer=prefer, local_convert=convert_local_to_markdown
        )
        diag(
            "convert.done",
            f"engine={outcome.engine or '-'} kind={outcome.kind} "
            f"ok={outcome.ok} degraded={outcome.degraded} escalated={outcome.escalated} "
            f"chars={len(outcome.text or '')}",
        )

        extra = {
            "engine": outcome.engine or None,
            "route": outcome.route(),
            "degraded": outcome.degraded,
            "escalated": outcome.escalated,
        }
        # 第三条路：页面图交还给 Agent 自己看。**need_agent_vision 是给模型看的信号**，
        # 它比任何自然语言 note 都可靠 —— Agent 扫一眼字段就知道该去 read_image。
        if outcome.page_images:
            extra["page_images"] = list(outcome.page_images)
            extra["need_agent_vision"] = True
        note = outcome.note()

        if not outcome.ok:
            # 失败也把已经拿到的正文交出去（例如「只处理了前 N 页」），
            # 但 ok=false 必须留着 —— 否则 Agent 会以为这份文档完整拿到了。
            payload = {
                "ok": False,
                "source": str(src),
                **extra,
                "error": outcome.error or "转换失败（没有更多信息）",
            }
            if note:
                payload["note"] = note
            if outcome.text and outcome.text.strip():
                payload.update(
                    truncate_or_dump(
                        outcome.text, max_chars, name_hint=src.stem or "converted"
                    )
                )
            return payload

        text = outcome.text or ""
        # `prefer="accuracy"` 走通时**没有正文**：产物就是那几张页面图。
        # 这时不能套用「正文 + 落盘/截断」那套字段（chars=0 会让 Agent 以为失败了），
        # 而是明确告诉它「去读这些图」。
        if not text.strip() and outcome.page_images:
            payload = {
                "ok": True,
                "source": str(src),
                "chars": 0,
                "lines": 0,
                "truncated": False,
                **extra,
            }
            if note:
                payload["note"] = note
            return payload

        if output_path and str(output_path).strip():
            dest = resolve_input_path(output_path, must_exist=False)
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(text, encoding="utf-8")
            payload = {
                "ok": True,
                "source": str(src),
                "path": str(dest),
                "chars": len(text),
                "lines": text.count("\n") + 1 if text else 0,
                "truncated": False,
                **extra,
            }
        else:
            dumped = truncate_or_dump(text, max_chars, name_hint=src.stem or "converted")
            payload = {"ok": True, "source": str(src), **dumped, **extra}
        if note:
            payload["note"] = note
        return payload

    return run_tool(work)


@dsh_bridge.tool(
    name="convert_files_to_markdown",
    description=(
        "批量把多个本机文档转成 Markdown。适合「把这个文件夹里的报告都整理成 md」"
        "这类一次性任务。**批量只走本地引擎**（不路由到云端）：免费 API 的 QPS 没有"
        "公开承诺，批量并发必踩限流；扫描件 / 图片请逐个调用 convert_file_to_markdown。"
        "单个文件失败**不会**中断整批：每条结果独立给出 ok / error，"
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
                    # 批量刻意钉死 prefer="fast"：只走本地，不上传、不限流（见工具描述）
                    prefer="fast",
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
            "router": {
                "fast": "本地 markitdown：Office/HTML/CSV，以及文本层干净、无表格的 PDF（不联网）",
                "structural": "云端版面模型 OXR：有表格/复杂版面/扫描件的 PDF，以及独立图片",
                "accuracy": "渲染页面图交回 Agent 自己读：多音标小语种、手写、需逐字准确（仅图片与扫描件）",
                "prefer": "auto（默认）/ fast / layout / accuracy —— 见 convert_file_to_markdown",
            },
            "note": (
                "转换在桥接常驻进程内完成。import markitdown 时会临时屏蔽 pandas，"
                "以免它拖入 numpy 的 C 扩展（本机每进程首次加载会挂约 10 分钟）；"
                "docx/pdf/pptx 等格式本就不需要 pandas，xlsx 由 openpyxl 原生处理。"
                "批量工具 convert_files_to_markdown 只走本地，不上传。"
            ),
        }
        info["cloud"] = _cloud_capabilities()
        info["agent_vision"] = _agent_vision_capabilities()
        if _CONVERTER_ERROR:
            info["error"] = _CONVERTER_ERROR
        return info

    return run_tool(work)


def _agent_vision_capabilities() -> dict:
    """第三条路的能力说明：页面图渲染到哪、留多久、怎么读。

    这条路**不依赖任何外部服务**，所以它永远可用（除非 pdfplumber 坏了）——
    与云端开关无关，这也是它比旧 VLM 方案更可靠的地方。
    """
    base = doc_route.page_pics_dir()
    try:
        import pdfplumber  # noqa: F401

        renderable = True
    except Exception:  # noqa: BLE001
        renderable = False
    return {
        "enabled": renderable,
        "prefer": "accuracy",
        "engine": "page-images",
        "page_pics_dir": str(base),
        "max_pages": doc_route.MAX_RENDER_PAGES,
        "dpi": doc_route.RENDER_DPI,
        "ttl_hours": doc_route.PAGE_PIC_TTL_SECONDS // 3600,
        "how": (
            "prefer=\"accuracy\" 时把页面渲染成 PNG 落盘，结果里给 page_images 绝对路径 + "
            "need_agent_vision=true；Agent 用 read_image 逐张读图、自己转写。"
            "不经任何 OCR，精度上限就是 Agent 自己的视觉能力。"
        ),
        "note": (
            "旧版这里配的是外部视觉大模型（DSH_VLM_* 三项 + 计费 API），已删除："
            "Agent 本身有视觉能力，再调一个多模态 API 属于重复建设。"
            if renderable
            else "pdfplumber 不可用，页面图渲染不可用；prefer=\"accuracy\" 会退回云端版面模型。"
        ),
        "cleanup": (
            f"每次渲染前自动删除 {doc_route.PAGE_PIC_TTL_SECONDS // 3600} 小时前的旧图"
            f"（目录：{base}）"
        ),
    }


def _cloud_capabilities() -> dict:
    """云端能力：只有 OXR 一节。**不联网、不发请求**，只读环境变量。

    用户问「为什么这份扫描件没走云端」时，这里就是答案：要么 `mode=off`，要么地址写错。
    """
    import os

    out: dict = {}
    mode = (os.environ.get("DSH_OXR_MODE", "auto") or "auto").strip().lower()
    oxr: dict = {
        "mode": mode,
        "enabled": mode != "off",
        "base_url": (os.environ.get("DSH_OXR_BASE_URL", "") or "https://free-oxr.somark.cn/api/v1"),
        "limits": "单次 ≤10MB / ≤20 页；超出按 20 页切片，最多 3 片（60 页）",
        "note": "免费服务无 SLA，请求会排队；额度/限流类错误不会重试。",
    }
    if mode == "off":
        oxr["note"] = (
            "DSH_OXR_MODE=off：云端版面模型已关闭，只走本地。"
            "注意 prefer=\"accuracy\" 仍可用 —— 它在本机渲染页面图，不联网。"
        )
    out["oxr"] = oxr
    return out
