# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""PPT 工具：python-pptx 读大纲 + 插图，Pillow 负责按比例算尺寸。

插图为什么要 Pillow：pptx 的 `add_picture` 只给**一个**尺寸参数，
另一个必须自己算。凭图片原始像素直接塞宽高会把图拉变形 —— 所以这里先
读图片尺寸，按幻灯片可用区域等比缩放。
"""

from __future__ import annotations

import dsh_bridge

from utils import ToolError, resolve_input_path, run_tool

# 16:9 幻灯片（13.333in × 7.5in = 12192000 × 6858000 EMU）留出边距后的可用区
SLIDE_W_EMU = 11_000_000
SLIDE_H_EMU = 5_600_000


@dsh_bridge.tool(
    name="read_pptx_outline",
    description=(
        "读取 PowerPoint（.pptx）的结构：每张幻灯片的标题、正文要点、备注、"
        "图片数量。当用户给了 PPT 让你「看内容 / 梳理大纲 / 总结结构」时用它。"
        "返回 {slides: [{index, title, bullets, notes, picture_count}], slide_count}。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "本机 .pptx 文件绝对路径"},
            "include_notes": {
                "type": "boolean",
                "description": "是否返回演讲者备注，默认 true",
            },
        },
        "required": ["file_path"],
    },
)
def read_pptx_outline(file_path: str, include_notes: bool = True) -> dict:
    def work():
        try:
            from pptx import Presentation
        except ImportError as exc:  # noqa: BLE001
            raise ToolError(f"python-pptx 不可用：{exc}。请重新执行「基本安装」。") from exc

        src = resolve_input_path(file_path)
        try:
            prs = Presentation(str(src))
        except Exception as exc:  # noqa: BLE001
            raise ToolError(f"打开 {src.name} 失败：{type(exc).__name__}: {exc}") from exc

        slides = []
        for i, slide in enumerate(prs.slides, start=1):
            title = ""
            bullets: list[str] = []
            notes = ""
            for shape in slide.shapes:
                if not getattr(shape, "has_text_frame", False):
                    continue
                is_title = False
                try:
                    is_title = shape == slide.shapes.title
                except Exception:  # noqa: BLE001
                    is_title = False
                text = shape.text_frame.text.strip()
                if not text:
                    continue
                if is_title and not title:
                    title = text
                else:
                    bullets.extend(ln.strip() for ln in text.splitlines() if ln.strip())
            if include_notes:
                try:
                    if slide.has_notes_slide:
                        notes = (slide.notes_slide.notes_text_frame.text or "").strip()
                except Exception:  # noqa: BLE001 — 备注页结构偶尔异常
                    notes = ""
            slides.append(
                {
                    "index": i,
                    "title": title,
                    "bullets": bullets[:100],
                    "notes": notes[:2000],
                    "picture_count": sum(
                        1 for s in slide.shapes if s.shape_type is not None and "PICTURE" in str(s.shape_type)
                    ),
                }
            )
        return {"source": str(src), "slide_count": len(slides), "slides": slides}

    return run_tool(work)


@dsh_bridge.tool(
    name="add_resized_image_to_pptx",
    description=(
        "往 .pptx 的指定幻灯片插入一张图片，**按原始比例等比缩放**到幻灯片可用区域"
        "（Pillow 负责读像素算比例；直接给宽高会把图拉变形）。"
        "适合「把这张截图/图表放进这份 PPT 的第 N 页」。"
        "slide_index 从 **1** 开始。max_width_ratio / max_height_ratio 可进一步限制占版比例。"
        "返回 {path, slide_index, width_emu, height_emu, original_size}。"
    ),
    parameters={
        "type": "object",
        "properties": {
            "pptx_path": {"type": "string", "description": "本机 .pptx 文件绝对路径（就地修改）"},
            "image_path": {"type": "string", "description": "本机图片绝对路径（png/jpg/…）"},
            "slide_index": {
                "type": "integer",
                "description": "第几张幻灯片，从 1 开始",
                "minimum": 1,
            },
            "max_width_ratio": {
                "type": "number",
                "description": "宽度最多占幻灯片可用区的比例，0~1，默认 1",
            },
            "max_height_ratio": {
                "type": "number",
                "description": "高度最多占幻灯片可用区的比例，0~1，默认 1",
            },
        },
        "required": ["pptx_path", "image_path", "slide_index"],
    },
)
def add_resized_image_to_pptx(
    pptx_path: str,
    image_path: str,
    slide_index: int,
    max_width_ratio: float = 1.0,
    max_height_ratio: float = 1.0,
) -> dict:
    def work():
        try:
            from PIL import Image
            from pptx import Presentation
        except ImportError as exc:  # noqa: BLE001
            raise ToolError(f"python-pptx / Pillow 不可用：{exc}。请重新执行「基本安装」。") from exc

        ppt = resolve_input_path(pptx_path)
        img = resolve_input_path(image_path)
        if not isinstance(slide_index, int) or slide_index < 1:
            raise ToolError("slide_index 从 1 开始（没有第 0 页）。")

        try:
            with Image.open(img) as im:
                px_w, px_h = im.size
        except Exception as exc:  # noqa: BLE001
            raise ToolError(f"读取图片 {img.name} 失败：{type(exc).__name__}: {exc}") from exc
        if px_w <= 0 or px_h <= 0:
            raise ToolError(f"图片 {img.name} 的尺寸异常（{px_w}×{px_h}）。")

        # 以「最大宽度」为基准算高，再各自夹到上限：这样横图不会被硬拉成正方形
        avail_w = SLIDE_W_EMU * max(0.05, min(float(max_width_ratio or 1.0), 1.0))
        avail_h = SLIDE_H_EMU * max(0.05, min(float(max_height_ratio or 1.0), 1.0))
        w = avail_w
        h = w * px_h / px_w
        if h > avail_h:
            h = avail_h
            w = h * px_w / px_h

        try:
            prs = Presentation(str(ppt))
            if slide_index > len(prs.slides):
                raise ToolError(
                    f"这份 PPT 只有 {len(prs.slides)} 页，要插入的是第 {slide_index} 页。"
                )
            slide = prs.slides[slide_index - 1]
            slide.shapes.add_picture(str(img), 0, 0, width=int(w), height=int(h))
            prs.save(str(ppt))
        except ToolError:
            raise
        except Exception as exc:  # noqa: BLE001
            raise ToolError(f"写入 PPT 失败：{type(exc).__name__}: {exc}") from exc

        return {
            "path": str(ppt),
            "slide_index": slide_index,
            "width_emu": int(w),
            "height_emu": int(h),
            "original_size": [px_w, px_h],
        }

    return run_tool(work)