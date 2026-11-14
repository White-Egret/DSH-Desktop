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

import re
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


# ---------- 连字归一化（U+FB00–FB06，2026-11 路由修复 D） ----------
#
# bake-off 实证：markitdown 的 PDF 路径留下未展开的兼容连字 —— 第7页.pdf 一页
# 就有 8 个（oﬃce / suﬀ. / ﬂag / ﬁrst），下游检索与词频会**静默漏词**
# （搜 "office" 搜不到 "oﬃce"，两边看着都是「读得通」）。
#
# 为什么是定向 translate 而不是全量 NFKC：NFKC 会把 CJK 全角标点/数字/拉丁
# （，！？；１２３ＡＢＣ）一并折叠成半角 —— 对中文文档是不可接受的副作用；
# 而下面这 7 个码位在正常 CJK / Office 文本里根本不出现，定向映射零副作用、O(n)。
_LIGATURE_TABLE = str.maketrans(
    {
        "\ufb00": "ff",   # ﬀ
        "\ufb01": "fi",   # ﬁ
        "\ufb02": "fl",   # ﬂ
        "\ufb03": "ffi",  # ﬃ
        "\ufb04": "ffl",  # ﬄ
        "\ufb05": "st",   # ﬅ（长 s + t）
        "\ufb06": "st",   # ﬆ
    }
)


def normalize_ligatures(text: str) -> str:
    """把 Unicode 兼容连字（U+FB00–FB06）展开成 ASCII 字母组合。

    只动这 7 个码位：全角标点、全角数字、CJK 一概不受影响（刻意不用 NFKC，
    理由见 _LIGATURE_TABLE 处注释）。
    """
    return (text or "").translate(_LIGATURE_TABLE)


# ---------- MHTML（.mhtml/.mht）：解 MIME 取主 HTML 再走 markitdown（2026-11-12 第三步） ----------
#
# 实证（Bacon.mhtml，Blink 保存，multipart/related + quoted-printable）：
# markitdown 没有 MHTML 转换器（`UnsupportedFormatException: No converter
# attempted… filetype not supported`）；直接改名 .html 也只会把 MIME 头和
# `=XX` 转义当 HTML 解析出垃圾。MHTML 的正文是 MIME 包裹的 HTML —— 先用
# **标准库 email** 解包（get_content() 自动解 quoted-printable/base64 并按
# charset 转 str），再把主文档 HTML 交给 markitdown 既有 HTML 管线。
# 全程本地、零新依赖；图片/CSS 资源部分不内嵌（本次只做正文）。

_MHTML_SUFFIXES = frozenset({".mhtml", ".mht"})


def _mhtml_main_html(data: bytes) -> str:
    """从 MHTML 字节里解出**主文档** HTML（纯标准库 email，无副作用）。

    多 HTML 部分（frame）时按优先级取主文档：
      1. 顶层 `Snapshot-Content-Location`（Blink 保存头）匹配 `Content-Location`
         的那份（忽略 #fragment）；
      2. 没有快照头或匹配不到 → 取**最大**的 text/html（帧/小页几乎总是更小，
         主文档几乎总是最大 —— 比「取第一个」稳健）。
    解不出 text/html → ToolError（明确报错，不静默返回空 —— 与第一步
    「0 字符当失败」同一纪律）。
    """
    import email
    from email import policy

    try:
        msg = email.message_from_bytes(data, policy=policy.default)
    except Exception as exc:  # noqa: BLE001 — 不是 MIME / 结构损坏
        raise ToolError(f"MHTML 解析失败：{type(exc).__name__}: {exc}") from exc
    snapshot = (msg.get("Snapshot-Content-Location") or "").strip()
    html_parts: list[tuple[str, str]] = []  # (content_location, html)
    for part in msg.walk():
        if part.get_content_type() != "text/html":
            continue
        try:
            content = part.get_content()  # 自动解 quoted-printable/base64 → str
        except Exception:  # noqa: BLE001 — 单个部分坏不影响其它部分
            continue
        if isinstance(content, str) and content.strip():
            html_parts.append(((part.get("Content-Location") or "").strip(), content))
    if not html_parts:
        raise ToolError(
            "MHTML 里没有可用的 text/html 部分：文件可能不是标准 MHTML（或已损坏）。"
        )
    if snapshot:
        snap_head = snapshot.split("#", 1)[0]
        for loc, content in html_parts:
            if loc and loc.split("#", 1)[0] == snap_head:
                return content
    return max(html_parts, key=lambda item: len(item[1]))[1]


def _mhtml_reconstruct_initials(html: str) -> str:
    """把「文件名即字母」的花体首字母图片还原成文本字母（用户反馈，2026-11-14）。

    老数字化站点常把每章首字母做成图片字形，文件名就是字母本身：
    Bacon.mhtml 实测 `q.gif` + 文本 "UID" = QUID、`m.gif` + "ETUUNT" = METUUNT
    （培根《论死亡》——不还原的话读者读到的是断词）。规则刻意保守：

      · `src` **紧贴斜杠后的基名**恰为 1–2 个字母（`/q.gif`、`/et.gif` 命中；
        `bar.gif` 三字母、`grsqr01.gif` 带数字、`bluesquare.gif`——若无斜杠锚定
        会被 `e.gif` 后缀误伤——等装饰图天然不命中，避免把分隔线换成 "AR" 的误报）；
      · 图片后面（允许隔空白/标签）必须紧跟字母数字——首字母天然连着单词；
      · 还原为大写（花体首字母即大写字形）。

    已知局限（如实声明）：≥3 个字母的首字形（如 EST）不会被还原——宁可少报
    也不冒 "BAR"→"AR" 误报的风险。
    """
    return _MHTML_INITIAL_RE.sub(lambda m: m.group(1).upper(), html)


_MHTML_INITIAL_RE = re.compile(
    r'<img\b[^>]*?src="[^"]*?/([A-Za-z]{1,2})\.(?:gif|png|jpe?g|bmp)"[^>]*>'  # 斜杠锚定基名=1–2字母
    r"(?=(?:\s|<[^>]*>)*[A-Za-z0-9\u00c0-\u024f])",                            # 且紧跟单词字符
    flags=re.I,
)


def _mhtml_to_markdown(src: Path) -> str:
    """MHTML → Markdown：解 MIME 取主 HTML，还原花体首字母，落临时 .html 走
    markitdown 的 HTML 管线（标题/表格/列表/pre 全部复用既有实现），不引入任何新依赖。"""
    import os
    import tempfile

    html = _mhtml_reconstruct_initials(_mhtml_main_html(src.read_bytes()))
    md = _converter()
    if md is None:
        raise ToolError(_CONVERTER_ERROR or "markitdown 不可用。")
    fd, tmp = tempfile.mkstemp(prefix="dsh_mhtml_", suffix=".html")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(html)
        result = md.convert_local(tmp)
    except Exception as exc:  # noqa: BLE001 — HTML 管线自身失败
        raise ToolError(f"MHTML 主文档转换失败：{type(exc).__name__}: {exc}") from exc
    finally:
        try:
            os.unlink(tmp)
        except OSError:
            pass
    text = getattr(result, "markdown", None)
    if text is None:
        raise ToolError("markitdown 没有返回内容（MHTML 主文档可能为空）。")
    text = normalize_ligatures(str(text))
    diag("convert.mhtml.done", f"{src.name} {len(text)} chars")
    return text


def convert_local_to_markdown(src: Path) -> str:
    """**本地 markitdown/openpyxl** 转换入口：任意本机文档 → Markdown，不联网、不上传。

    三路路由里它是所有路线的最后一级兜底（`doc_route.convert_any` 把本函数注入）；
    2026-11-12 第二步起，fast 路的 DOCX/CSV/PDF 先经 `convert_fast_anydoc_first`
    （anydoc 优先），本函数是它们的**兜底**与 HTML/PPTX/JSON/XML/epub/邮件的
    **唯一**执行者 —— 所以它必须自给自足：不依赖 doc_route 的任何状态，也不吞异常
    （抛 ToolError 由上层判断这次失败算不算「还能交付」）。

    xlsx/xlsm 走 openpyxl 原生路径，MHTML 先解 MIME 取主 HTML（见
    _mhtml_to_markdown），其余走 markitdown；`.xls` 明确拒绝
    （读它要 pandas+xlrd，而 pandas 会拖入本机会挂死的 numpy C 扩展）。
    """
    suffix = src.suffix.lower()
    if suffix in _MHTML_SUFFIXES:
        # MHTML 必须在 markitdown 通用路径**之前**分流：它没有对应转换器
        # （实测 UnsupportedFormatException），且 MIME 头/=XX 转义不能当 HTML 解
        return _mhtml_to_markdown(src)
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
    # 连字归一化（2026-11 修复 D）：markitdown 的 PDF 路径会留下 U+FB0x
    # （oﬃce 这类），下游按 ASCII 检索/数词会静默漏词。只走 markitdown 的分支：
    # xlsx 走 openpyxl 原生路径，没有这个已实证的问题，不顺手扩大范围。
    text = normalize_ligatures(text)
    diag("convert.markitdown.done", f"{len(text)} chars")
    return text


# ---------- anydoc：fast 路首选引擎（2026-11-12 第二步） ----------
#
# firecrawl-anydoc（import 名 anydoc）：Rust 实现的本地文档 → Markdown 引擎，
# 零 Python 依赖的 abi3 wheel，不受坑 A（numpy）/坑 B（magika→onnxruntime）影响。
# bake-off（anydoc-bakeoff-report.md §7/§9）结论：DOCX/CSV/干净文本层 PDF/LaTeX
# 用它首选（标题层级与表格保真更好、快 38–52 倍、失败会说话）；PPTX/HTML 刻意
# 不给它（乱序/丢边界、不支持），XLSX 不给它（15 位有效数字截断）。
# 版本锁定 0.2.4：0.2.x 行为随小版本变化（连字丢失在 0.2.3→0.2.4 被修复），
# 升级须重跑 bake-off（见 PY_BASIC_PKGS 的 ==0.2.4 钉版）。

_ANYDOC = None
_ANYDOC_ERROR: str | None = None


def _anydoc():
    """惰性 import anydoc。装不上/损坏时返回 None 并记录原因 ——
    fast 路据此整体回落 markitdown，行为与第一步完全一致。"""
    global _ANYDOC, _ANYDOC_ERROR
    if _ANYDOC is not None or _ANYDOC_ERROR is not None:
        return _ANYDOC
    try:
        import anydoc
    except Exception as exc:  # noqa: BLE001
        diag("anydoc.import-fail", f"{type(exc).__name__}: {str(exc)[:160]}")
        _ANYDOC_ERROR = f"anydoc 不可用：{type(exc).__name__}: {exc}"
        return None
    _ANYDOC = anydoc
    return _ANYDOC


# 只有这三个后缀允许进 anydoc（bake-off §7 的硬约束）：
#   .docx  标题层级/表格保真更好（synth_docx：5/5 标题 + 真表头 vs markitdown 6 行假表）
#   .csv   输出等价且更快；CSV 无魔数，必须 to_markdown_bytes(data, "csv")
#   .pdf   干净文本层/LaTeX（花括号保真抽检与 NeedsOcr 降级见 doc_route）
# .pptx（乱序/丢边界）、.html（UnsupportedError）、.xlsx（15 位截断）**不进**。
_ANYDOC_FAST_SUFFIXES = frozenset({".docx", ".csv", ".pdf"})


def convert_fast_anydoc_first(src: Path) -> "tuple[str, str]":
    """fast 路的 anydoc 优先入口：返回 `(engine, text)`。

    分流（engine 词表：anydoc / markitdown / openpyxl）：
      · .docx / .csv / .pdf → 先试 anydoc；anydoc 报错（Unsupported/Malformed/…）
        → 回落 markitdown；.pdf 且报 NeedsOcrError → 抛 `doc_route.NeedsOcrSignal`
        （**降级信号不是错误**，由 doc_route 决定转 structural/accuracy；
        .docx/.csv 的 NeedsOcrError 与其它错误同权处理 —— OXR 不收 Office，
        升级没有意义，直接 markitdown 兜底）。
      · 其余后缀 → 原样走 convert_local_to_markdown（HTML/PPTX 归 markitdown，
        xlsx/xlsm 归 openpyxl，.xls 明确拒绝）。
    anydoc 不可用（未装/损坏）→ 整体回落 markitdown，行为与第一步一致。
    连字归一化对 anydoc 输出统一生效（0.2.4 已自展开，这里是不变式兜底）。
    """
    suffix = src.suffix.lower()
    if suffix not in _ANYDOC_FAST_SUFFIXES:
        engine = "openpyxl" if suffix in _XLSX_SUFFIXES else "markitdown"
        return (engine, convert_local_to_markdown(src))

    ad = _anydoc()
    if ad is None:
        diag("anydoc.missing", f"{src.name} 回落 markitdown")
        return ("markitdown", convert_local_to_markdown(src))
    try:
        if suffix == ".csv":
            # CSV 无魔数：显式传格式名（bake-off §7 实测的调用方式）
            text = ad.to_markdown_bytes(src.read_bytes(), "csv")
        else:
            text = ad.to_markdown(str(src))
    except ad.NeedsOcrError as exc:
        pages = list(getattr(exc, "pages", []) or [])
        raise doc_route.NeedsOcrSignal(
            f"anydoc NeedsOcrError（pages={pages[:5]}，其解析器拿不到可用文本）"
        ) from exc
    except Exception:  # noqa: BLE001 — Unsupported/Malformed/Encrypted/… → markitdown 兜底
        diag("anydoc.fail-fallback", f"{src.name} → markitdown")
        return ("markitdown", convert_local_to_markdown(src))
    text = normalize_ligatures(str(text))
    diag("convert.anydoc.done", f"{src.name} {len(text)} chars")
    return ("anydoc", text)


@dsh_bridge.tool(
    name="convert_file_to_markdown",
    description=(
        "把一个本机文档转成 Markdown，自动选引擎（三路路由）："
        "**fast 路本地引擎 anydoc 优先**（Rust，不联网）：DOCX / CSV / 干净文本层 PDF / "
        "LaTeX PDF 由 anydoc 首选（标题层级与表格保真更好、比 markitdown 快 38–52 倍），"
        "HTML 与 PPTX 仍由 markitdown 转换（anydoc 不支持 HTML、PPTX 会乱序丢边界），"
        "MHTML（.mhtml/.mht）先解 MIME 取主文档 HTML 再走 markitdown（全程本地），"
        "XLSX/XLSM 走 openpyxl 原生（浮点全精度）；anydoc 对 PDF 报 NeedsOcrError"
        "（其解析器拿不到文本）时是**降级信号**——自动转交 structural/accuracy，不报错；"
        "anydoc 已知会静默丢 LaTeX 花括号，故 PDF 另有花括号保真抽检：文本层有花括号"
        "而 anydoc 输出丢了一半以上 → 自动回退 markitdown 并在 note 说明。"
        "**含表格或复杂版面的 PDF、扫描件 PDF、以及独立图片** 走云端版面模型 OXR"
        "（会把文件上传到 SoMark 免费服务，单次 ≤10MB/≤20 页，无 key、免费）；"
        "**当版面模型不够好时（多音标小语种、手写、极小字号、需要逐字准确引用），"
        "传 prefer=\"accuracy\"：工具会把页面渲染成 PNG 并把绝对路径交给你，"
        "由你**自己用 read_image 读图转写**——这条最准，因为不经任何 OCR，"
        "也不需要任何 API Key。**注意：这条路返回的是图片路径而不是正文**。"
        "**早期印本（文本层含长 s ſ 或早期连字码点）auto 会自动转 accuracy** 并在 note "
        "附专用转写规则（长 s 绝不认 f、连字/缩写展开、多音标希腊文、双栏语言-栏位逐页"
        "按图判断）——OXR 对这类文献实证失效，不要给它。"
        "云端不可用时自动回落本地并在 degraded/note 里标注，不会返回一份空文件。"
        "**注意：OXR / 本地的结果都是 OCR 草稿，通顺不等于正确** —— 专名与译名"
        "（台湾/大陆译名差异大，勿按常识「纠正」）、漏字（句子仍通顺但少了字）、"
        "形近字（彫/膨、兒/尼）、数字与日期都可能出错；需要逐字准确时用 "
        "prefer=\"accuracy\" 对照原图核对。"
        "双栏/多栏论文、带公式的试卷、发票等版面敏感文档传 prefer=\"layout\""
        "（auto 不做通用多栏检测——页眉/通栏标题会破坏判据，误判率高于收益）；"
        "**竖排文字（竖排中文/日文、古籍）与 OCR 藏字文本层（扫描后套了隐藏识别层）"
        "的 PDF auto 已能识别**（字符坐标行列比 + 字体名两个信号，2026-11 修复），"
        "命中时自动走云端版面模型 OXR、禁走本地，无需再声明 layout；"
        "只有字序错乱等侦察信号抓不到的版面异常仍需手动传 layout。"
        "离线或只要纯文本传 prefer=\"fast\"。"
        "只接受本机绝对路径，不接受 URL。"
        "返回 {ok, engine, route, source, path, chars, preview}"
        "（engine 是 anydoc / markitdown / openpyxl / oxr / page-images / "
        "markitdown-fallback；"
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
                    "layout = 强制云端版面模型 OXR（双栏论文、公式试卷、发票，"
                    "以及侦察信号抓不到的版面异常）；"
                    "accuracy = 把页面渲染成图交给你自己读（多音标小语种、手写、逐字引用、"
                    "竖排/OCR 藏字文本层、早期印本；真正的干净文本层 PDF 仍走本地）。"
                    "竖排中文/日文与 OCR 藏字文本层的 PDF auto 已能自动识别并走云端"
                    "（字符坐标行列比 + 字体名两个信号，2026-11 修复）；"
                    "早期印本（文本层含长 s/早期连字）auto 自动转 accuracy 并附专用转写规则"
                    "（2026-11-12 第三步）—— OXR 对早期印本实证失效；"
                    "只有字序错乱等抓不到的版面异常仍请传 layout。"
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
            src,
            prefer=prefer,
            local_convert=convert_local_to_markdown,
            fast_convert=convert_fast_anydoc_first,
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
                "xlsx/xlsm": "openpyxl 原生转换（不经 markitdown/anydoc，不需要 pandas，浮点全精度）",
                "docx": "anydoc 优先（Rust 本地；markitdown/mammoth 兜底）",
                "pptx": "markitdown（python-pptx）—— anydoc 会乱序、丢 slide 边界，刻意不换",
                "csv": "anydoc 优先（to_markdown_bytes 显式格式；markitdown 兜底，输出等价）",
                "pdf": "anydoc 优先（干净文本层/LaTeX；花括号保真抽检，NeedsOcrError 降级云端）；markitdown 兜底",
                "html": "markitdown（anydoc 不支持 HTML，实测 UnsupportedError）",
                "mhtml/mht": "markitdown（先解 MIME 取主文档 HTML——标准库 email，全程本地；并把「文件名即字母」的花体首字母图片还原成文本（q.gif→QUID）；资源部分不内嵌）",
                "json/xml/epub/Outlook 邮件/zip": "markitdown",
                "xls": "不支持（旧版格式，读它要 pandas+xlrd；请另存为 .xlsx）",
            },
            "router": {
                "fast": "本地引擎：DOCX/CSV/干净文本层 PDF/LaTeX 由 anydoc 优先（Rust 本地、快 38–52 倍），HTML/PPTX 由 markitdown，XLSX 由 openpyxl（不联网）",
                "structural": "云端版面模型 OXR：有表格/复杂版面/扫描件的 PDF，以及独立图片",
                "accuracy": "渲染页面图交回 Agent 自己读：多音标小语种、手写、需逐字准确（仅图片与扫描件）",
                "early_print": (
                    "早期印本（16–18 世纪）：文本层含长 s（ſ，U+017F）或早期连字码点"
                    "（U+FB00–FB06）即判为早期印本，auto/accuracy 自动转 accuracy 并附专用"
                    "转写规则（长 s 绝不认 f、连字/缩写展开、多音标希腊文、双栏语言-栏位逐页"
                    "按图判断）。依据：OXR 对 old-publication.pdf（波菲利《Isagoge》早期印本）"
                    "实证失效——Ariſtoteli→Ariftoteli、PRÆFATIO→PRAFATIO、希腊文成乱码。"
                    "局限：纯扫描（无文本层）的早期印本检不出，靠 Agent 看渲染图或显式 prefer。"
                    "prefer=\"layout\" 仍可强制 OXR（会附风险提示）；prefer=\"fast\" 尊重覆盖。"
                ),
                "prefer": "auto（默认）/ fast / layout / accuracy —— 见 convert_file_to_markdown",
                "untrusted_text_layer": (
                    "竖排文字与 OCR 藏字文本层（GlyphLessFont / Hidden*OCR 字体）的 PDF "
                    "auto 已能识别（字符坐标行列比 + 字体名两个信号，2026-11 路由修复），"
                    "命中即走云端 OXR、禁走 fast —— 字多不再等于文本层可信。"
                    "此时强制 prefer=\"fast\" 仍走本地，但结果会带明确的不可信提示。"
                ),
                "layout_required": (
                    "双栏/多栏（auto 不做通用多栏检测——页眉/通栏标题会破坏判据）"
                    "与字序错乱等侦察信号抓不到的版面异常，仍需**显式**传 layout；"
                    "竖排与 OCR 藏字层不再需要（auto 已能识别，2026-11 修复）。"
                ),
                "ocr_is_draft": (
                    "OXR 与本地产出的都是 OCR 草稿，**通顺不等于正确**。实测同一份竖排扫描件，"
                    "layout 的正文通顺却含四处错字：笛卡尼→笛卡兒、拿破崙二世→拿破崙一世、"
                    "大膨刻家→大彫刻家、軍家→軍事家（漏字）。"
                    "错误集中在四类：专名与译名（勿按常识纠正，台湾/大陆译名差异大）、"
                    "漏字（句子仍通顺，最难发现）、形近字、数字与日期。"
                    "需要逐字准确时用 prefer=\"accuracy\" 对照原图核对。"
                    "另：云端结果的**结尾**可能幻觉出原文没有的收尾句、或丢掉最后一条（实测），"
                    "引用级用途请对照页面末尾核对。"
                ),
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
        info["anydoc"] = _anydoc_capabilities()
        if _CONVERTER_ERROR:
            info["error"] = _CONVERTER_ERROR
        return info

    return run_tool(work)


def _anydoc_capabilities() -> dict:
    """anydoc（firecrawl-anydoc）的能力与可用性。

    **不 import 模块本体**：find_spec 只查存在性，版本从 dist-info 元数据取 ——
    「capabilities 是纯静态报告」的纪律不破（它曾是 ~600 秒挂死的受害者）。
    """
    import importlib.metadata
    import importlib.util

    out: dict = {
        "enabled": False,
        "engine": "anydoc",
        "formats": "docx / csv / 干净文本层 PDF / LaTeX PDF（fast 路首选）",
        "note": "未安装时 fast 路自动整体回落 markitdown（行为与 2026-11 第一步一致）。",
    }
    try:
        if importlib.util.find_spec("anydoc") is not None:
            out["enabled"] = True
            try:
                out["version"] = importlib.metadata.version("firecrawl-anydoc")
            except Exception:  # noqa: BLE001 — 元数据缺失不影响可用性报告
                pass
    except Exception:  # noqa: BLE001
        pass
    out["brace_check"] = (
        "已知缺陷：anydoc 会静默丢 LaTeX 花括号（arxiv_lora 实测文本层 20 个 → 输出 0 个，"
        "`{edwardhu`→`fedwardhu`、`}` 直接消失）。缓解：PDF 文本层（前 3 页）花括号 ≥ "
        f"{doc_route.BRACE_MIN_TEXT_LAYER} 个时做保真抽检，anydoc 输出不足一半 → "
        "自动回退 markitdown（它保留花括号）并写明 note；轻微差异不触发，避免误报。"
    )
    out["needs_ocr"] = (
        "anydoc 对 PDF 报 NeedsOcrError（其解析器拿不到文本，比「有无文本层」严格）"
        "时是降级信号：auto 自动转 structural(OXR)、accuracy 转 accuracy、"
        "fast 尊重覆盖回落 markitdown。"
    )
    return out


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


# ---------- 自测（无网络、不 import markitdown） ----------


def self_test() -> list[str]:
    """纯函数自测，返回失败清单（空 = 全绿）。供 doc_route.py --self-test 调用。

    刻意只测纯函数（连字归一化）：不触发 `_converter()`，也就不会
    import markitdown / numpy —— 断网、裸环境照样全绿。
    """
    failures: list[str] = []

    def check(name: str, got, want) -> None:
        if got != want:
            failures.append(f"md.{name}: 期望 {want!r}，实际 {got!r}")

    # bake-off 第7页.pdf 实测出现过的四个词
    check("连字 oﬃce", normalize_ligatures("oﬃce"), "office")
    check("连字 suﬀ.", normalize_ligatures("suﬀ."), "suff.")
    check("连字 ﬂag", normalize_ligatures("ﬂag"), "flag")
    check("连字 ﬁrst", normalize_ligatures("ﬁrst"), "first")
    # 七个码位逐个展开（不用拼接大字面量，避免肉眼对错）
    for src_ch, want in (
        ("\ufb00", "ff"),
        ("\ufb01", "fi"),
        ("\ufb02", "fl"),
        ("\ufb03", "ffi"),
        ("\ufb04", "ffl"),
        ("\ufb05", "st"),
        ("\ufb06", "st"),
    ):
        check(f"连字 U+{ord(src_ch):04X}", normalize_ligatures(src_ch), want)
    # 归一化后 U+FB0x 计数必须为 0
    sample = "oﬃce suﬀ. ﬂag ﬁrst ﬀﬁﬂﬃﬄﬅﬆ"
    normalized = normalize_ligatures(sample)
    check(
        "连字 计数归零",
        sum(normalized.count(ch) for ch in "\ufb00\ufb01\ufb02\ufb03\ufb04\ufb05\ufb06"),
        0,
    )
    # 边界与 CJK 安全：全角标点/数字/拉丁一概不动（NFKC 会动，这里不许动）
    check("连字 空串", normalize_ligatures(""), "")
    check(
        "连字 中文无副作用",
        normalize_ligatures("中文，标点！；１２３ＡＢＣ"),
        "中文，标点！；１２３ＡＢＣ",
    )

    # anydoc 分流表（2026-11-12 第二步）：只允许这三个后缀进 anydoc；
    # HTML/PPTX/XLSX 是 bake-off §7 的硬约束，必须在排除集里。
    for s in (".docx", ".csv", ".pdf"):
        check(f"anydoc 优先 {s}", s in _ANYDOC_FAST_SUFFIXES, True)
    for s in (".pptx", ".html", ".htm", ".xlsx", ".xlsm", ".json", ".xml", ".xls"):
        check(f"不走 anydoc {s}", s in _ANYDOC_FAST_SUFFIXES, False)

    # —— MHTML 解包（2026-11-12 第三步，纯标准库 email）——
    # 构造一份带 quoted-printable 的 multipart/related：主 HTML + CSS 资源部分。
    # =63='c'、=C3=B6='ö'（utf-8）、=73='s' —— 解码后不得残留任何 =XX 转义。
    mhtml_bytes = (
        b"From: <Saved by Blink>\r\n"
        b"Snapshot-Content-Location: https://example.com/main\r\n"
        b"MIME-Version: 1.0\r\n"
        b'Content-Type: multipart/related; boundary="----=_Part_1"\r\n'
        b"\r\n"
        b"------=_Part_1\r\n"
        b"Content-Type: text/html; charset=utf-8\r\n"
        b"Content-Transfer-Encoding: quoted-printable\r\n"
        b"Content-Location: https://example.com/main\r\n"
        b"\r\n"
        b"<html><body><h1>Ba=63on</h1><p>s=C3=B6me nece=73=73e</p></body></html>\r\n"
        b"------=_Part_1\r\n"
        b"Content-Type: text/css\r\n"
        b"Content-Transfer-Encoding: base64\r\n"
        b"\r\n"
        b"Ym9keSB7Y29sb3I6IHJlZH07\r\n"
        b"------=_Part_1--\r\n"
    )
    main = _mhtml_main_html(mhtml_bytes)
    check("MHTML qp 解码", ("Bacon" in main) and ("necesse" in main) and ("ö" in main), True)
    check("MHTML 无 qp 转义", ("Ba=63on" not in main) and ("=73" not in main), True)
    # 多 HTML 部分：快照头匹配的才是主文档（另一个更大但 Content-Location 不同）
    mhtml_two = (
        b"Snapshot-Content-Location: https://example.com/real\r\n"
        b"MIME-Version: 1.0\r\n"
        b'Content-Type: multipart/related; boundary="----=_Part_2"\r\n'
        b"\r\n"
        b"------=_Part_2\r\n"
        b"Content-Type: text/html\r\n"
        b"Content-Location: https://example.com/frame\r\n"
        b"\r\n"
        b"<html>" + b"<p>frame filler</p>" * 20 + b"</html>\r\n"
        b"------=_Part_2\r\n"
        b"Content-Type: text/html\r\n"
        b"Content-Location: https://example.com/real\r\n"
        b"\r\n"
        b"<html><body><p>main document</p></body></html>\r\n"
        b"------=_Part_2--\r\n"
    )
    check(
        "MHTML 快照头优先于大小",
        ("main document" in _mhtml_main_html(mhtml_two))
        and ("frame filler" not in _mhtml_main_html(mhtml_two)),
        True,
    )
    # 无快照头 → 取最大的 text/html
    mhtml_nosnap = mhtml_two.replace(b"Snapshot-Content-Location: https://example.com/real\r\n", b"")
    check("MHTML 无快照头取最大", "frame filler" in _mhtml_main_html(mhtml_nosnap), True)
    # 解不出 text/html → 明确报错（不静默返回空）
    try:
        _mhtml_main_html(
            b"Content-Type: multipart/related; boundary=\"----=_B\"\r\n\r\n"
            b"------=_B\r\nContent-Type: image/png\r\n\r\nAAAA\r\n------=_B--\r\n"
        )
    except ToolError:
        pass
    else:
        failures.append("md.MHTML 无 text/html: 应当抛 ToolError")

    # —— 花体首字母还原（2026-11-14 用户反馈）——
    # 基名即字母的图片字形 → 文本字母；装饰图/分隔线不得误转。
    html_q = '<p><img src="https://x.org/essays/q.gif" width="89">UID est veritas?</p>'
    check("首字母 q.gif→Q", _mhtml_reconstruct_initials(html_q), "<p>QUID est veritas?</p>")
    html_m = '<p><img src="https://x.org/essays/m.gif">ETUUNT homines mortem</p>'
    check("首字母 m.gif→M", _mhtml_reconstruct_initials(html_m), "<p>METUUNT homines mortem</p>")
    html_et = '<p><img src="https://x.org/essays/et.gif">jam</p>'
    check("首字母 双字母 et.gif→ET", _mhtml_reconstruct_initials(html_et), "<p>ETjam</p>")
    # 误报防线：三字母基名（分隔线 bar.gif）、带数字（grsqr01.gif）、单词基名
    # （bluesquare.gif）都不命中；首字母后不紧跟单词字符也不命中。
    for name, src, tail in (
        ("bar.gif 不还原", "https://x.org/essays/bar.gif", "正文"),
        ("grsqr01.gif 不还原", "https://x.org/essays/grsqr01.gif", "正文"),
        ("bluesquare.gif 不还原", "https://x.org/essays/bluesquare.gif", "正文"),
        ("后无单词字符不还原", "https://x.org/essays/q.gif", " <p>分隔用</p>"),
    ):
        tag = f'<p><img src="{src}">{tail}</p>'
        check(f"首字母还原守卫 {name}", _mhtml_reconstruct_initials(tag), tag)

    return failures
