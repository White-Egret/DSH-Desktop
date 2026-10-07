# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""markitdown 工具：任意办公/文档格式 → Markdown。

只用 `MarkItDown().convert_local(...)`（**最窄的转换入口**）：显式走本地文件路径，
不接受 URL —— 见 utils.resolve_input_path 的说明（SSRF / 远程内容注入）。
`markitdown[all]` 里的 Azure / 音频转写 extras 需要额外凭据或外部程序（ffmpeg），
这里一律**优雅降级**：缺依赖时返回一句人话，而不是让整个工具组崩掉。
"""

from __future__ import annotations

from pathlib import Path

import dsh_bridge

from utils import (
    DEFAULT_MAX_CHARS,
    ToolError,
    resolve_input_path,
    run_tool,
    truncate_or_dump,
)

_CONVERTER = None
_CONVERTER_ERROR: str | None = None


def _converter():
    """惰性构造 MarkItDown（进程内只造一个，跨调用复用）。

    延迟到第一次真正调用才 import：桥接进程启动时就要把 manifest 交出去，
    万一 markitdown 没装好，不该让**整个桥接**起不来（其它工具仍然可用）。
    """
    global _CONVERTER, _CONVERTER_ERROR
    if _CONVERTER is not None or _CONVERTER_ERROR is not None:
        return _CONVERTER
    try:
        from markitdown import MarkItDown

        _CONVERTER = MarkItDown()
    except Exception as exc:  # noqa: BLE001
        _CONVERTER_ERROR = (
            f"markitdown 不可用：{type(exc).__name__}: {exc}。"
            "请在首选项「Python 环境」里重新点一次「基本安装」。"
        )
    return _CONVERTER


@dsh_bridge.tool(
    name="convert_file_to_markdown",
    description=(
        "把一个本机文档（docx / pptx / xlsx / xls / pdf / html / csv / json / xml / "
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
        md = _converter()
        if md is None:
            raise ToolError(_CONVERTER_ERROR or "markitdown 不可用。")
        src = resolve_input_path(file_path)
        try:
            result = md.convert_local(str(src))
        except Exception as exc:  # noqa: BLE001 — 缺 ffmpeg / 凭据 / 加密 PDF 都在这里
            raise ToolError(
                f"转换 {src.name} 失败：{type(exc).__name__}: {exc}。"
                "（音频转写需要 ffmpeg、Azure 相关转换需要凭据；纯文档格式不受影响。）"
            ) from exc
        text = getattr(result, "markdown", None)
        if text is None:
            raise ToolError(f"markitdown 没有为 {src.name} 返回内容（可能是不支持的格式）。")
        text = str(text)

        # 显式指定了输出路径：直接写全文，再回一个短摘要（调用方要的是文件，不是正文）
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
        payload = {"source": str(src)}
        payload.update(dumped)
        return payload

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
        "报告当前 Python 环境里 markitdown 的可用转换器与缺失的 extras。"
        "在批量转换失败、或用户问「为什么这个 pdf 转不了」时先调它，"
        "拿到的是一份明确的缺依赖清单，而不是猜测。"
    ),
    parameters={"type": "object", "properties": {}},
)
def markitdown_capabilities() -> dict:
    def work():
        info: dict = {"markitdown_installed": False}
        try:
            import importlib.metadata as md

            info["markitdown_version"] = md.version("markitdown")
            info["markitdown_installed"] = True
        except Exception:  # noqa: BLE001
            info["markitdown_version"] = None

        md_conv = _converter()
        info["usable"] = md_conv is not None
        if not info["usable"]:
            info["error"] = _CONVERTER_ERROR
            return info

        # 逐个探测「装了包但缺系统程序」的 extras：ffmpeg / 外部转换器这类
        info["extras"] = {}
        for label, module in (
            ("pdf", "pdfminer"),
            ("docx", "mammoth"),
            ("xlsx", "openpyxl"),
            ("pptx", "pptx"),
            ("xls", "xlrd"),
            ("outlook_msg", "extract_msg"),
            ("audio_transcription", "markitdown.audio_transcription"),
            ("youtube_transcription", "youtube_transcript_api"),
        ):
            try:
                __import__(module)
                info["extras"][label] = {"available": True}
            except Exception as exc:  # noqa: BLE001
                info["extras"][label] = {
                    "available": False,
                    "reason": f"{type(exc).__name__}: {exc}",
                }
        return info

    return run_tool(work)