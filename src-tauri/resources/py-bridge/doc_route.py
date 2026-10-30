# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""三路路由：本地 markitdown / 云端版面模型 OXR / **Agent 亲自看**（渲染页面图）。

**三条路各管一段，谁都不许越界冒充另一个**：

| 路线 | 强项 | 弱项 |
|---|---|---|
| 本地 markitdown | Office / HTML / CSV；文本层完整的 PDF；0.1 s | 图片不做 OCR（实测 0 字符）；表格拉平成散行 |
| OXR（1.3B 版面模型） | 版面顺序、表格、公式、中英扫描件 | 小语种字形混淆（希腊文实测 10% 字母错） |
| **Agent 亲自看** | 最准：真读像素，签名/日期/手写都能看见 | 每页一张图，上下文有限，只适合少量页 |

**第三路为什么不是「外部视觉大模型」**（2026-10-30 修订，推翻了本模块最初的 VLM 设计）：
最初的实现配 `DSH_VLM_*` 三项、调一个付费多模态 API。实测证明这个前提是错的 ——
**Agent 本身就有视觉能力**（`read_image` 直接读像素），希腊文样本当场逐字读出，
而 OXR 在同一份文件上有 10% 字母错。所以第三层从「另一个模型」改成「Agent 自己」：

    convert_file_to_markdown 把页面渲染成 PNG 落到 page-pics\\，
    在结果里给出图片绝对路径 + need_agent_vision=true，
    Agent 用 read_image 自己看。

这比调外部 API 更准（没有第二道有损转换）、更省（零成本）、少一个配置项。
`vlm_client.py` 已随之删除；`DSH_VLM_*` 三个环境变量不再被读取。

本模块**只有纯函数 + 编排**，不注册任何 `@dsh_bridge.tool`：工具面在
`markitdown_tools.py` 里，这里负责「这份文件该交给谁、失败了怎么退」。
唯一的例外是 `--self-test`（无网络、无 pytest，直接 `python doc_route.py --self-test`）。

**为什么路由判据要做成纯函数**（`decide` / `script_profile` / `postprocess`）：
路由错了的代价不对称 —— 该走云端的走了本地，用户拿到一份 0 字符的 .md 还以为是
空文档；该走本地的走了云端，白花一次上传与等待。纯函数才能被表驱动钉住。

**为什么不做「自动识别多栏」**：多栏判断靠抽样页字符坐标，页眉、通栏标题、图注
都会破坏判据，误判代价是「左右栏串行交错」。所以只提供可声明的 `prefer="layout"`，
交给懂语境的 Agent 声明（方案 §3）。
"""

from __future__ import annotations

import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path

from utils import ToolError, bridge_home, diag

# ---------- 常量（方案 §4 / §7.4） ----------

IMAGE_SUFFIXES = frozenset({".png", ".jpg", ".jpeg", ".webp", ".bmp", ".tif", ".tiff"})
LOCAL_SUFFIXES = frozenset(
    {
        ".docx", ".pptx", ".xlsx", ".xlsm", ".html", ".htm",
        ".csv", ".json", ".xml", ".epub", ".msg",
    }
)

# 抽样页的「均字数」阈值。依据（方案 §1.1）：扫描件 0 字/页、表格样本 227 字/页、
# 纯文本 2237 字/页 —— 80 落在 0 与 227 之间，两边都有余量。
SCANNED_AVG_CHARS = 80
# 有图且文本层薄到这个程度，同样按扫描件处理（图文混排的封面页 / 盖章页）
SCANNED_WITH_IMAGES_AVG_CHARS = 300

# 升级规则 2 的判据：超过三成字母属「第三类文字」，就附上原图让 Agent 自己看（方案 §4）
NON_LATIN_CJK_RATIO = 0.30

# 本地兜底「能不能交付」的下限：低于这个字符数就不当成功（方案 §5「质量感知兜底」）。
# 扫描件的本地产出是 0~几十字符，把这个数字当成功返回等于骗 Agent「文档是空的」。
MIN_LOCAL_CHARS = 80

VALID_PREFER = ("auto", "fast", "layout", "accuracy")

# ---- 第三条路：把页面渲染成图，交给 Agent 自己看（2026-10-30） ----
#
# 落点固定在桥接目录的 page-pics\ 下：DSH_PY_BRIDGE_HOME 已经在子进程环境里，
# 不需要为它改 Rust / bundle 的任何配置（少一跳传递就少一个失败点）。
PAGE_PICS_DIRNAME = "page-pics"
# 单次最多渲染这么多页：每页一张图进上下文，多了既费 token 也没意义
MAX_RENDER_PAGES = 10
# 渲染分辨率。150 DPI 是「正文可读 + 文件不过大」的折中：
# 实测扫描件单页 150 DPI 约 100KB，正文与页边行号都清晰。
RENDER_DPI = 150
# 图片保留时长：超过就删。定时清理靠「每次渲染前扫一遍」实现 ——
# 桥接进程没有常驻定时器（它只在工具调用时活着），用调用点触发既简单又够用。
PAGE_PIC_TTL_SECONDS = 24 * 3600


@dataclass(frozen=True)
class PdfProbe:
    """PDF 侦察结果（只读，不落盘）。"""

    pages: int
    avg_chars: float
    images: int
    tables: int


@dataclass
class RouteOutcome:
    """一次转换的结果与诊断。

    `text` 由调用方（markitdown_tools）负责落盘或截断，本模块只负责页面图的落盘
    （那是它自己的产物，路径必须由它给出）。
    """

    ok: bool
    kind: str = "fast"          # fast | structural | accuracy（与 decide() 的返回值同词表）
    reason: str = ""            # 为什么走这条：image / native-format / scanned / tables …
    engine: str = ""            # markitdown | oxr | page-images | markitdown-fallback
    text: str = ""
    error: str = ""
    escalated: bool = False     # 仅当「自动体检」把结果补上了页面图供校对
    degraded: bool = False      # 交付档位低于路由期望（走了本地 / 该升级没升上去）
    probe_ms: int = 0
    pages: int = 0
    # 页面图绝对路径。非空时调用方必须把它交给 Agent，并置 need_agent_vision。
    page_images: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def route(self) -> dict:
        """给 Agent 看的诊断块（方案 §7.5）。"""
        return {
            "kind": self.kind,
            "reason": self.reason,
            "escalated": self.escalated,
            "probe_ms": self.probe_ms,
        }

    def note(self) -> str:
        """多条说明合并成一句：Agent 读一行就够，不必解析数组。"""
        seen: list[str] = []
        for n in self.notes:
            if n and n not in seen:
                seen.append(n)
        return "；".join(seen)


# ---------- 纯函数：路由判据 ----------


def decide(suffix: str, probe: "PdfProbe | None", prefer: str) -> str:
    """返回 'fast' | 'structural' | 'accuracy'。永不返回 None：每一步都要有结论。"""
    return decide_route(suffix, probe, prefer)[0]


def decide_route(
    suffix: str, probe: "PdfProbe | None", prefer: str
) -> tuple[str, str]:
    """`decide` 的带理由版本 —— 理由会进结果里的 `route.reason`，给 Agent 解释用。

    判据逐条对应方案 §4，**顺序就是优先级**，改任何一条都要连带看注释：
    显式档 > 图片 > 原生格式 > 非 PDF > layout > accuracy 的三段判断 > 自动判据。
    """
    suffix = (suffix or "").lower()

    if prefer == "fast":
        return "fast", "prefer-fast"
    if suffix in IMAGE_SUFFIXES:
        # 图片只有云端能救：本地 markitdown 对图片实测 0 字符
        if prefer == "accuracy":
            return "accuracy", "image-accuracy"
        return "structural", "image"
    if suffix in LOCAL_SUFFIXES:
        # OXR / 渲染都不收 Office，本地最拿手
        return "fast", "native-format"
    if suffix != ".pdf":
        return "fast", "not-pdf"
    if prefer == "layout":
        return "structural", "prefer-layout"
    if prefer == "accuracy":
        # 只有扫描件才值得渲染成图：干净文本层本来就是精确的（那是原生文本，不是
        # OCR 出来的），渲染成图再让 Agent 读反而更慢、更贵、还可能看错。
        # 这条同时是「防止为了精度把纯文本 PDF 也走一遍视觉」的回归用例。
        if probe is None:
            return "fast", "accuracy-probe-failed"
        if probe.avg_chars >= SCANNED_AVG_CHARS and probe.tables == 0:
            return "fast", "accuracy-clean-text-layer"
        if probe.avg_chars >= SCANNED_AVG_CHARS:
            return "structural", "accuracy-text-layer-with-tables"
        return "accuracy", "accuracy-scanned"
    if probe is None:
        # 侦察失败（文件损坏 / pdfplumber 缺失）不等于「该上传」，宁可本地
        return "fast", "probe-failed"
    if probe.avg_chars < SCANNED_AVG_CHARS or (
        probe.images > 0 and probe.avg_chars < SCANNED_WITH_IMAGES_AVG_CHARS
    ):
        # 路由结论一样（都是 structural），但**理由要分开说**：真扫描件与
        # 「有图 + 文本偏薄」在用户眼里是两件事，混成一个 "scanned" 会让人以为
        # 自己那份有正文的 PDF 被当成扫描件了。有表格时优先报 tables。
        return "structural", "tables" if probe.tables > 0 else "scanned"
    if probe.tables > 0:
        return "structural", "tables"
    return "fast", "clean-text-layer"


def normalize_prefer(prefer: "str | None") -> str:
    """把 Agent 传来的档位归一化；不认识的值直接报错，而不是悄悄当 auto。

    悄悄回退到 auto 会让「我明明声明了 accuracy 却没走这条路」变成一个查不出来的
    谜 —— 参数拼错时人需要一句明确的「可选值是这四个」。
    """
    text = (prefer or "auto").strip().lower()
    if not text:
        text = "auto"
    if text not in VALID_PREFER:
        raise ToolError(
            f"prefer 只接受 {' / '.join(VALID_PREFER)}，收到 {prefer!r}。"
            "auto = 自动判；fast = 只用本地；layout = 强制云端版面模型；"
            "accuracy = 强制视觉大模型（仅对图片与扫描件有效）。"
        )
    return text


# ---------- 纯函数：脚本体检（升级规则 2） ----------


def script_profile(text: str) -> dict[str, int]:
    """按书写系统数一下字母。CJK 与拉丁都由 OXR 处理得很好，其余都是它的弱项。"""
    out = {"latin": 0, "cjk": 0, "other": 0}
    for ch in text or "":
        if not ch.isalpha():
            continue
        o = ord(ch)
        if o < 0x250:  # 含扩展拉丁
            out["latin"] += 1
        elif 0x2E80 <= o <= 0x9FFF or 0xAC00 <= o <= 0xD7FF:
            out["cjk"] += 1
        else:  # 希腊 / 西里尔 / 希伯来 / 阿拉伯 / 天城…
            out["other"] += 1
    return out


def needs_vlm(text: str) -> bool:
    """文本里「非拉丁、非 CJK 的字母」占比是否超过三成。"""
    p = script_profile(text)
    total = p["latin"] + p["cjk"] + p["other"]
    return total > 0 and p["other"] / total > NON_LATIN_CJK_RATIO


def other_ratio(text: str) -> float:
    """`needs_vlm` 用的那个比例，写进 note 让 Agent 知道「体检判了多狠」。"""
    p = script_profile(text)
    total = p["latin"] + p["cjk"] + p["other"]
    return (p["other"] / total) if total else 0.0


# ---------- 纯函数：后处理 ----------

_IMG = re.compile(r"!\[[^\]]*\]\([^)]*\)")


def postprocess(md: str) -> tuple[str, int]:
    """剥掉会过期的云端图片链接（实测 OXR 的 image="none" 并未抑制它们）。

    返回 (正文, 去掉的张数)。**不下载图片**：那要引入 image-persistence 一整套
    （目录、命名、重试、清理），而本工具的输出是给 Agent 读的文本。
    """
    stripped, n = _IMG.subn("", md or "")
    return stripped, n


# ---------- 第三条路：页面图（渲染给 Agent 自己看） ----------


def page_pics_dir() -> Path:
    """页面图目录：`<桥接目录>\\page-pics\\`。

    桥接目录由宿主通过 `DSH_PY_BRIDGE_HOME` 传入（`utils.bridge_home`），
    所以这里不需要额外的配置项 —— 少一跳传递就少一个失败点。
    """
    return bridge_home() / PAGE_PICS_DIRNAME


def _writable_fallback_dir() -> "Path | None":
    """桥接目录不可写时的退路：用户临时目录。

    **为什么需要它**：桥接目录在 `%APPDATA%` 下，正常情况下可写；但在
    受管环境（企业策略、只读用户目录）、或本机沙箱限制下可能被拒。
    这时整条 `prefer="accuracy"` 会失效 —— 而它恰恰是「云端不可用时的最后手段」，
    不该因为一个目录权限而彻底没有。退到临时目录至少能让 Agent 看到图。

    找不到可写目录就返回 None，调用方据此如实报告「渲染不可用」。
    """
    import tempfile

    try:
        base = Path(tempfile.gettempdir()) / "dsh-desktop-py-bridge" / PAGE_PICS_DIRNAME
        base.mkdir(parents=True, exist_ok=True)
        probe = base / f".probe_{os.getpid()}"
        probe.write_text("", encoding="utf-8")
        probe.unlink(missing_ok=True)
        return base
    except OSError:
        return None


def _resolve_page_pics_dir() -> "Path | None":
    """挑一个真正可写的页面图目录：桥接目录优先，失败退临时目录。"""
    primary = page_pics_dir()
    try:
        primary.mkdir(parents=True, exist_ok=True)
        probe = primary / f".probe_{os.getpid()}"
        probe.write_text("", encoding="utf-8")
        probe.unlink(missing_ok=True)
        return primary
    except OSError as exc:
        diag("page-pics.primary-unwritable", f"{primary} | {exc}")
    fallback = _writable_fallback_dir()
    if fallback is not None:
        diag("page-pics.fallback", str(fallback))
    return fallback


def purge_old_page_pics(now: "float | None" = None) -> int:
    """删掉过期的页面图，返回删除张数。

    **为什么用「调用点触发」而不是常驻定时器**：桥接进程只在工具调用时活着
    （没有请求就没有事件循环），装一个 `threading.Timer` 既留不住也不可靠。
    每次渲染前扫一遍目录，效果等价于「定时清理」，且不需要任何后台线程 ——
    这是本模块唯一合理的清理时机，因为只有它知道这些文件是自己的产物。

    桥接目录与兜底临时目录**都扫**：曾经落到兜底目录的图也要清，否则那条路会
    成为垃圾堆积的死角。

    删不掉的文件（正被 read_image 读、权限不足）**静默跳过**：
    清理失败绝不能反过来让一次转换失败。
    """
    cutoff = (time.time() if now is None else now) - PAGE_PIC_TTL_SECONDS
    removed = 0
    for base in _candidate_dirs():
        if not base.is_dir():
            continue
        try:
            entries = list(base.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                if not entry.is_file() or entry.suffix.lower() not in (".png", ".jpg", ".jpeg"):
                    continue
                if entry.stat().st_mtime < cutoff:
                    entry.unlink()
                    removed += 1
            except OSError:
                continue
    if removed:
        diag("page-pics.purge", f"删除 {removed} 张过期页面图")
    return removed


def _candidate_dirs() -> list[Path]:
    """可能存放页面图的目录（主目录 + 兜底目录），不保证存在。"""
    import tempfile

    return [
        page_pics_dir(),
        Path(tempfile.gettempdir()) / "dsh-desktop-py-bridge" / PAGE_PICS_DIRNAME,
    ]


def _safe_stem(name: str) -> str:
    """把文件名压成可落盘的一段：去掉路径分隔符与 Windows 保留字符。

    页面图是给人/给 Agent 看的产物，文件名要能看出**来自哪份文档**，
    所以保留原名的可读部分，只替换真正会出问题的字符。
    """
    stem = re.sub(r'[<>:"/\\|?*\x00-\x1f]', "_", name or "document")
    stem = stem.strip(" .") or "document"
    return stem[:60]


def render_pages(src: Path, *, max_pages: int = MAX_RENDER_PAGES, dpi: int = RENDER_DPI) -> list[str]:
    """把文档的页面渲染成 PNG，返回绝对路径列表（按页序）。

    支持两类输入：
      · **PDF** —— 用 pdfplumber 逐页 `to_image()`（已实测可用；pypdfium2 也在，但
        pdfplumber 是既有依赖，不再引入第二条渲染路径）；
      · **独立图片** —— 原样复制一份到 page-pics（Agent 可以直接读原文件，
        但复制过来能让「这次的产物」集中在一处，且同样受 TTL 清理约束）。

    抽不出 / 渲染失败一律返回**已成功的那些页**（可能为空），不抛错：
    调用方据此决定是「部分可用」还是「这条路走不通」。
    """
    base = _resolve_page_pics_dir()
    if base is None:
        diag("page-pics.no-writable-dir", "桥接目录与临时目录都不可写")
        return []

    purge_old_page_pics()

    stamp = time.strftime("%Y%m%d_%H%M%S")
    stem = _safe_stem(src.stem)
    suffix = src.suffix.lower()
    out: list[str] = []

    if suffix in IMAGE_SUFFIXES:
        # 独立图片：复制一份，文件名带上时间戳（同一张图不同次转换互不覆盖）
        dest = base / f"{stem}_{stamp}{suffix if suffix != '.jpeg' else '.jpg'}"
        try:
            dest.write_bytes(src.read_bytes())
            out.append(str(dest))
        except OSError as exc:
            diag("page-pics.copy-fail", str(exc)[:160])
        return out

    if suffix != ".pdf":
        return []

    try:
        import pdfplumber  # noqa: PLC0415 — 惰性：没有它只是渲染不了，不该影响转换
    except Exception:  # noqa: BLE001
        diag("page-pics.no-pdfplumber", "无法渲染页面图")
        return []

    try:
        with pdfplumber.open(str(src)) as pdf:
            total = len(pdf.pages)
            for idx in range(min(max_pages, total)):
                dest = base / f"{stem}_p{idx + 1}_{stamp}.png"
                try:
                    page = pdf.pages[idx]
                    page.to_image(resolution=dpi).save(str(dest), format="PNG")
                except Exception as exc:  # noqa: BLE001 — 单页渲染失败不影响其它页
                    diag("page-pics.page-fail", f"p{idx + 1}: {type(exc).__name__}: {str(exc)[:120]}")
                    continue
                out.append(str(dest))
    except Exception as exc:  # noqa: BLE001 — 加密 / 损坏
        diag("page-pics.open-fail", f"{type(exc).__name__}: {str(exc)[:160]}")
        return out
    if total > max_pages:
        diag("page-pics.truncated", f"只渲染前 {max_pages}/{total} 页")
    return out


# ---------- 侦察（唯一会碰文件的函数） ----------


def probe_pdf(path: Path) -> "PdfProbe | None":
    """抽样侦察一个 PDF：页数、前 3 页均字数、图片数、前 2 页表格数。

    只读、不落盘，实测四份样本 0.04~0.21 s（方案 §1.3）。
    **任何失败都返回 None 而不是抛错** —— 侦察失败只说明「判不了」，
    路由会退回本地，绝不能让一次侦察把整次转换弄崩。
    """
    try:
        import pdfplumber
    except Exception:  # noqa: BLE001 — 没装就判不了，走本地
        return None
    try:
        with pdfplumber.open(str(path)) as pdf:
            pages = len(pdf.pages)
            if pages == 0:
                return PdfProbe(pages=0, avg_chars=0.0, images=0, tables=0)
            sample = [pdf.pages[i] for i in range(min(3, pages))]
            texts = [(p.extract_text() or "").strip() for p in sample]
            images = 0
            for p in sample:
                try:
                    images += len(p.images)
                except Exception:  # noqa: BLE001 — 单个页面的图片表坏了不影响其它页
                    pass
            tables = 0
            for p in sample[:2]:  # 表格探测最慢，只做 2 页
                try:
                    tables += len(p.find_tables())
                except Exception:  # noqa: BLE001 — 探测失败不算有表格
                    pass
    except Exception as exc:  # noqa: BLE001 — 加密 / 损坏 / 根本不是 PDF
        diag("probe.fail", f"{type(exc).__name__}: {str(exc)[:160]}")
        return None
    return PdfProbe(
        pages=pages,
        avg_chars=sum(map(len, texts)) / max(1, len(texts)),
        images=images,
        tables=tables,
    )


# ---------- 编排 ----------

# 云端总开关。`DSH_OXR_MODE=off` 按方案 §7.6 的语义是「完全关闭云端」：
# 两个云端引擎一起停 —— 隐私开关不能只关一半，否则用户以为没上传、其实 OXR 还在传。
# 注意：`off` **不影响** `prefer="accuracy"` —— 那条路渲染在本机、不联网。
def cloud_disabled() -> bool:
    import os

    return (os.environ.get("DSH_OXR_MODE", "auto") or "auto").strip().lower() == "off"


def _cloud_block_reason() -> str:
    return "云端引擎已关闭（DSH_OXR_MODE=off）" if cloud_disabled() else ""


def _import_oxr():
    """惰性 import：OXR 模块坏了不该让整组文档工具消失。"""
    try:
        import oxr_client

        return oxr_client
    except Exception as exc:  # noqa: BLE001
        diag("route.oxr.import-fail", f"{type(exc).__name__}: {str(exc)[:160]}")
        return None


def _describe_failure(exc: BaseException) -> str:
    """把客户端异常转成一句人话（客户端负责把「连不上」与「服务返回错误」分开）。"""
    kind = getattr(exc, "kind", "")
    msg = getattr(exc, "message", "") or str(exc)
    table = {
        "network": "连不上云端服务",
        "http": "云端服务返回错误",
        "rate": "云端限流",
        "timeout": "云端处理超时",
        "unsupported": "云端不支持或无法解析该文件",
        "too-large": "文件超过云端上限",
        "quota": "云端额度已用尽",
        "auth": "云端 API Key 无效或已过期",
        "server": "云端服务内部错误",
    }
    head = table.get(kind, "云端转换失败")
    return f"{head}：{msg}" if msg else head


def convert_any(src: Path, *, prefer: str = "auto", local_convert) -> RouteOutcome:
    """按路由把 `src` 转成 Markdown。

    `local_convert` 由 markitdown_tools 注入（`convert_local_to_markdown`）——
    刻意不做成模块内 import：那会形成 markitdown_tools ↔ doc_route 的循环依赖，
    而把「本地怎么转」的知识留在一处也更好维护。
    """
    prefer = normalize_prefer(prefer)
    suffix = src.suffix.lower()

    t0 = time.perf_counter()
    probe = probe_pdf(src) if suffix == ".pdf" else None
    probe_ms = int(round((time.perf_counter() - t0) * 1000))

    kind, reason = decide_route(suffix, probe, prefer)
    out = RouteOutcome(
        ok=False,
        kind=kind,
        reason=reason,
        probe_ms=probe_ms,
        pages=probe.pages if probe else 0,
    )
    detail = (
        f"{suffix or '（无扩展名）'} prefer={prefer} -> {kind}/{reason}"
        f" probe={probe_ms}ms"
        + (f" pages={probe.pages} avg={probe.avg_chars:.0f} tables={probe.tables}" if probe else "")
    )
    diag("route.decide", detail)

    if kind == "fast":
        return _run_fast(out, src, local_convert)
    if kind == "structural":
        return _run_structural(out, src, local_convert)
    return _run_accuracy(out, src, local_convert)


# ---- 路线一：本地 ----


def _run_fast(out: RouteOutcome, src: Path, local_convert) -> RouteOutcome:
    try:
        text = local_convert(src)
    except Exception as exc:  # noqa: BLE001 — 本地自己报错就是最终错误
        out.error = f"{exc}" if isinstance(exc, ToolError) else f"{type(exc).__name__}: {exc}"
        return out
    out.ok = True
    out.engine = "markitdown"
    out.text = text
    return out


# ---- 路线二：structural（OXR；体检不过则附页面图给 Agent 校对） ----


def _run_structural(out: RouteOutcome, src: Path, local_convert) -> RouteOutcome:
    blocked = _cloud_block_reason()
    if blocked:
        out.notes.append(blocked)
        return _local_fallback(out, src, local_convert, why=blocked)

    oxr = _import_oxr()
    if oxr is None:
        return _local_fallback(out, src, local_convert, why="云端引擎模块不可用")

    try:
        raw = oxr.parse_file(src, pages=out.pages or None)
    except Exception as exc:  # noqa: BLE001 — 客户端把所有失败都包成 OxrError
        why = _describe_failure(exc)
        diag("route.oxr.fail", f"{getattr(exc, 'kind', '?')} | {str(exc)[:200]}")
        return _local_fallback(out, src, local_convert, why=why)

    text, stripped = postprocess(raw)
    if stripped:
        out.notes.append(f"已省略 {stripped} 张图片（云端链接会过期）")
    if not text.strip():
        diag("route.oxr.empty", src.name)
        return _local_fallback(out, src, local_convert, why="云端返回了空内容")

    out.ok = True
    out.engine = "oxr"
    out.text = text
    _maybe_escalate(out, src)
    return out


def _maybe_escalate(out: RouteOutcome, src: Path) -> None:
    """自动体检：OXR 结果里第三类文字占比过高 → 把原页渲染出来供 Agent 校对。

    这条规则是被希腊文样本逼出来的（方案 §1.2）：OXR 换温度、放大、重试都不动
    那个天花板。但**结论与方案初稿不同** —— 解法不是「换一个付费视觉模型」，
    而是「把原图交还给本来就有眼睛的 Agent」：

    · 实测同一份 Greek.jpg，OXR 有 10.1% 字母错，Agent 直接读像素**逐字正确**；
    · 且这条路径零成本、零配置，不会因为「没配 key」而整条路不可用。

    渲染失败**不推翻**已经拿到的 OXR 结果，只标注 degraded 并写清原因。
    """
    if not needs_vlm(out.text):
        return
    ratio = other_ratio(out.text)
    images = render_pages(src)
    if not images:
        out.degraded = True
        out.notes.append(
            f"文本含大量非拉丁、非中日韩字母（体检：other={ratio:.0%}），"
            "版面模型在这类字形上精度有限；页面图渲染失败，无法自动附上原图校对"
        )
        return
    out.page_images = images
    out.escalated = True
    out.notes.append(
        f"版面模型在非拉丁文字上精度不足（体检：other={ratio:.0%}），"
        f"已附上 {len(images)} 张页面图 —— 请用 read_image 自己核对"
    )


# ---- 路线三：accuracy（Agent 亲自看） ----


def _run_accuracy(out: RouteOutcome, src: Path, local_convert) -> RouteOutcome:
    """`prefer="accuracy"`：把页面渲染成图，交给 **Agent 自己**读。

    这条路**不受云端开关影响**（`DSH_OXR_MODE=off` 关的是上传，不是 Agent 的眼睛），
    也不需要任何 key —— 渲染在本机完成，Agent 用 `read_image` 读本地文件。

    与旧实现的关键差别：这里**不产生文本**。页面图的像素交给 Agent，
    由 Agent 自己转写 —— 少一道有损转换，精度上限就是 Agent 自己的视觉能力。
    """
    images = render_pages(src)
    if not images:
        # 渲染不出来（pdfplumber 缺失 / 加密 / 纯矢量页）才退到云端，并说清原因
        out.notes.append("页面图渲染失败（pdfplumber 不可用、文件加密或无可渲染页）")
        oxr = _import_oxr()
        if oxr is None or _cloud_block_reason():
            out.notes.append(_cloud_block_reason() or "云端引擎模块不可用")
            return _local_fallback(
                out, src, local_convert, why="页面图渲染失败且云端不可用"
            )
        try:
            raw = oxr.parse_file(src, pages=out.pages or None)
        except Exception as exc:  # noqa: BLE001
            out.notes.append(f"云端版面模型也失败（{_describe_failure(exc)}）")
            return _local_fallback(out, src, local_convert, why=_describe_failure(exc))
        text, stripped = postprocess(raw)
        if stripped:
            out.notes.append(f"已省略 {stripped} 张图片（云端链接会过期）")
        if not text.strip():
            return _local_fallback(out, src, local_convert, why="云端版面模型返回了空内容")
        out.ok = True
        out.text = text
        out.engine = "oxr"
        out.degraded = True
        out.notes.append("本次为版面模型结果（页面图渲染不可用），精度较低")
        return out

    out.ok = True
    out.engine = "page-images"
    out.page_images = images
    if out.pages and out.pages > len(images):
        out.notes.append(
            f"只渲染了前 {len(images)}/{out.pages} 页（单次上限 {MAX_RENDER_PAGES} 页）"
        )
    out.notes.append(
        f"已把 {len(images)} 张页面图交给你自己读：请用 read_image 逐张查看，"
        "按页序拼接为 Markdown。这条路不经任何 OCR，精度取决于你自己的视觉"
    )
    return out


# ---- 质量感知兜底（唯一不能省的分支） ----


def _local_fallback(
    out: RouteOutcome, src: Path, local_convert, *, why: str
) -> RouteOutcome:
    """云端不可用时回落本地，但**不返回空文件**（方案 §5）。

    扫描件失败时返回一份 0 字符的 .md 比报错更糟：Agent 会当成「文档是空的」
    继续往下走，后面所有推理都建立在「这份文件没有内容」这个假事实上。
    """
    try:
        text = local_convert(src)
    except Exception as exc:  # noqa: BLE001
        reason = str(exc) if isinstance(exc, ToolError) else f"{type(exc).__name__}: {exc}"
        out.ok = False
        out.error = "；".join(
            [n for n in out.notes if n] + [f"云端：{why}", f"本地：{reason}"]
        )
        return out

    body = (text or "").strip()
    if len(body) < MIN_LOCAL_CHARS:
        out.ok = False
        out.text = text or ""
        out.error = "；".join(
            [n for n in out.notes if n]
            + [
                f"云端：{why}",
                f"本地只提到 {len(body)} 个字符 —— 这份文档需要云端引擎"
                "（扫描件/图片请检查网络与 DSH_OXR_BASE_URL，或在首选项里重新执行「基本安装」）",
            ]
        )
        return out

    out.ok = True
    out.engine = "markitdown-fallback"
    out.degraded = True
    out.text = text
    out.notes.append(f"云端引擎不可用（{why}），已回落本地转换，版面结构可能丢失")
    return out


# ---------- 自测（无网络、无 pytest） ----------


def _self_test() -> int:
    """表驱动自测：`python doc_route.py --self-test`。

    刻意不 import pdfplumber / requests：判据全是纯函数，断网也该全绿。
    """
    failures: list[str] = []

    def check(name: str, got, want) -> None:
        if got != want:
            failures.append(f"{name}: 期望 {want!r}，实际 {got!r}")

    def check_true(name: str, cond: bool) -> None:
        if not cond:
            failures.append(f"{name}: 期望为真，实际为假")

    clean = PdfProbe(pages=3, avg_chars=2237, images=0, tables=0)
    scanned = PdfProbe(pages=1, avg_chars=0, images=0, tables=0)
    table = PdfProbe(pages=3, avg_chars=227, images=0, tables=1)
    thick_with_images = PdfProbe(pages=3, avg_chars=250, images=2, tables=0)

    # decide()：方案 §10 的表驱动清单
    check("png/auto", decide(".png", None, "auto"), "structural")
    check("png/accuracy", decide(".png", None, "accuracy"), "accuracy")
    check("docx/auto", decide(".docx", None, "auto"), "fast")
    check("pdf 扫描件", decide(".pdf", scanned, "auto"), "structural")
    check("pdf 纯文本", decide(".pdf", clean, "auto"), "fast")
    check("pdf 表格", decide(".pdf", table, "auto"), "structural")
    check("pdf 图文混排薄文本", decide(".pdf", thick_with_images, "auto"), "structural")
    check("probe=None", decide(".pdf", None, "auto"), "fast")
    check("prefer=fast 覆盖一切", decide(".pdf", scanned, "fast"), "fast")
    check("prefer=layout", decide(".pdf", clean, "layout"), "structural")
    check("accuracy+干净文本层", decide(".pdf", clean, "accuracy"), "fast")
    check("accuracy+有表格", decide(".pdf", table, "accuracy"), "structural")
    check("accuracy+扫描件", decide(".pdf", scanned, "accuracy"), "accuracy")
    check("accuracy+侦察失败", decide(".pdf", None, "accuracy"), "fast")
    check("xls 走本地", decide(".xls", None, "auto"), "fast")
    check("无扩展名", decide("", None, "auto"), "fast")

    # 理由码：给 Agent 解释用，改了判据要一起改
    check("理由 png", decide_route(".png", None, "auto")[1], "image")
    check("理由 扫描件", decide_route(".pdf", scanned, "auto")[1], "scanned")
    check("理由 干净文本层", decide_route(".pdf", clean, "auto")[1], "clean-text-layer")
    # 有图 + 薄文本 + 有表格：结论仍是 structural，但理由要报 tables 而不是 scanned
    check(
        "理由 图文混排带表格",
        decide_route(".pdf", PdfProbe(pages=3, avg_chars=227, images=1, tables=1), "auto")[1],
        "tables",
    )
    check("理由 图文混排无表格", decide_route(".pdf", thick_with_images, "auto")[1], "scanned")

    # normalize_prefer
    check("prefer 大小写", normalize_prefer("  ACCURACY "), "accuracy")
    check("prefer 空值", normalize_prefer(None), "auto")
    try:
        normalize_prefer("turbo")
    except ToolError:
        pass
    else:
        failures.append("normalize_prefer: 非法档位应当抛 ToolError")

    # script_profile / needs_vlm
    greek = (
        "Πρόκειται τῇδε τῇ συγγραφῇ ἐπιστήμην τινὰ πορίσασθαι τῆς τε φύσεως "
        "καὶ τῶν ὑπ' αὐτῆς γινομένων"
    )
    check_true("needs_vlm(希腊文)", needs_vlm(greek))
    check("needs_vlm(中文)", needs_vlm("物業收回通知書 請於七日內繳清欠款"), False)
    check("needs_vlm(英文表格)", needs_vlm("| Item | Amount |\n|---|---|\n| Rent | 100 |"), False)
    check("needs_vlm(空串)", needs_vlm(""), False)
    check("needs_vlm(纯标点)", needs_vlm("123 456 --- |||"), False)
    check("script_profile(希腊文)", script_profile("αβγ")["other"], 3)
    check("script_profile(拉丁)", script_profile("abc")["latin"], 3)
    check("script_profile(中文)", script_profile("物业")["cjk"], 2)
    check("other_ratio(空串)", other_ratio(""), 0.0)

    # postprocess：剥图片链接，保留普通链接
    md = "正文 ![x](https://a/b.png) 还有 ![](https://c/d.png) 和 [文字](https://e/f)\n"
    stripped, n = postprocess(md)
    check("postprocess 计数", n, 2)
    check_true("postprocess 剥掉图片链接", "![" not in stripped)
    check_true("postprocess 保留普通链接", "[文字](https://e/f)" in stripped)
    check("postprocess 空串", postprocess(""), ("", 0))

    # 编排层的纯逻辑：云端总开关
    old = os.environ.get("DSH_OXR_MODE")
    try:
        os.environ["DSH_OXR_MODE"] = "off"
        check_true("cloud_disabled(off)", cloud_disabled())
        os.environ["DSH_OXR_MODE"] = "auto"
        check("cloud_disabled(auto)", cloud_disabled(), False)
    finally:
        if old is None:
            os.environ.pop("DSH_OXR_MODE", None)
        else:
            os.environ["DSH_OXR_MODE"] = old

    # 页面图：目录名、文件名清洗、过期清理（用临时 mtime 造「老文件」验证 TTL）
    check("页面图目录名", PAGE_PICS_DIRNAME, "page-pics")
    check("文件名清洗(路径分隔符)", _safe_stem("a/b\\c:d*e?.pdf".replace(".pdf", "")), "a_b_c_d_e_")
    check("文件名清洗(空)", _safe_stem(""), "document")
    check("文件名清洗(过长截断)", len(_safe_stem("x" * 200)), 60)

    base = page_pics_dir()
    try:
        base.mkdir(parents=True, exist_ok=True)
        fresh = base / "_selftest_fresh.png"
        stale = base / "_selftest_stale.png"
        fresh.write_bytes(b"x")
        stale.write_bytes(b"x")
        old_ts = time.time() - (PAGE_PIC_TTL_SECONDS + 60)
        os.utime(stale, (old_ts, old_ts))
        removed = purge_old_page_pics()
        check_true("TTL 清理删掉了过期文件", not stale.exists())
        check_true("TTL 清理保留了新文件", fresh.exists())
        check_true("TTL 清理有计数", removed >= 1)
        fresh.unlink(missing_ok=True)
    except OSError as exc:  # 桥接目录不可写时跳过（不该让纯函数自测因此变红）
        diag("selftest.page-pics.skip", str(exc)[:120])

    # 兄弟模块的纯函数（同样是断网可跑的自测）
    for module_name in ("oxr_client",):
        try:
            mod = __import__(module_name)
        except Exception as exc:  # noqa: BLE001
            failures.append(f"{module_name} 导入失败：{type(exc).__name__}: {exc}")
            continue
        run = getattr(mod, "self_test", None)
        if run is None:
            continue
        try:
            more = run()
        except Exception as exc:  # noqa: BLE001
            failures.append(f"{module_name}.self_test 抛错：{type(exc).__name__}: {exc}")
            continue
        failures.extend(more)

    if failures:
        print(f"FAIL {len(failures)} 项：")
        for f in failures:
            print(f"     {f}")
        return 1
    print("OK   doc_route / oxr_client 纯函数自测全部通过（含页面图 TTL 清理）")
    return 0


if __name__ == "__main__":
    import sys

    if "--self-test" in sys.argv[1:]:
        raise SystemExit(_self_test())
    print("用法：python doc_route.py --self-test（本模块不注册工具，由 markitdown_tools 调用）")
    raise SystemExit(2)
