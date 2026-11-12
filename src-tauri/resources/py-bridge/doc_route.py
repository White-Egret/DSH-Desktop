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
但「文本层不可信」的两类脏文本层**有可靠信号**，不在其列（2026-11 路由修复）：
竖排（字符按列堆叠，distinct_y/distinct_x 比值极高）与 OCR 藏字层（文本层字体是
GlyphLessFont / Hidden*OCR）—— 两者命中即禁走本地 fast；通用多栏依旧不判。
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

# —— 「文本层不可信」判据（2026-11 路由修复，bake-off 实测标定）——
#
# 原判据「avg_chars ≥ 80 且 tables == 0 → 干净文本层 → fast」把**有文本层**当成了
# **文本层正确**。两个最脏的样本都有文本层，却被送进 fast，markitdown 产出整页假表格：
#   · 中文竖排件：文本层 1155 字/页 → 176 行假表格，关键串命中 0/30；
#   · 双栏拉丁词典（Souter）：文本层 3881 字 → 52 行假表格，左右栏逐行合并、列序全错。
# 所以补两个**可靠**信号（probe_pdf 计算，decide_route 命中即降级、禁走 fast）：
#
#   · vertical：竖排时字符按列堆叠 —— 每列共享同一 x0 → distinct_x（列数）少、
#     distinct_y（每列字数）多。本机实测（x0/top 按 ~2pt 归并）：竖排件比值
#     5.4~5.5，横排干净件 0.05~0.57，阈值 3.0 两侧各有近 2 倍余量。
#     通用「多栏」依旧不判（页眉/通栏标题会破坏判据，见模块 docstring），
#     只判「字符按列堆叠」这一种结构。
#   · ocr_font：文本层字体名是 OCR 藏字 → 文本层是识别引擎贴上去的，不是排版
#     软件写上去的，坐标序与列序都不可信。实测命中 GlyphLessFont（Souter）、
#     HiddenHorzOCR / HiddenVertOCR（竖排件）；干净件是 MBaskerville /
#     NimbusRomNo9L / CMR / Noto-Sans-SC 等真实排版字体。
VERTICAL_RATIO = 3.0
# 列数下限：≥50 字符且比值 ≥3 的横排页面根本不存在（横排字符 x0 必然铺满页宽），
# 这个下限只为挡住「所有字符挤在同一 x」之类的退化抽取，不承担正常判别。
VERTICAL_MIN_COLUMNS = 2
# 页面字符数下限：字符太少的页面不做行列统计（噪声占主导，判不出也判不准）
VERTICAL_MIN_PAGE_CHARS = 50
# OCR 藏字体的名字片段（统一小写后子串匹配）
OCR_FONT_MARKERS = ("glyphless", "hidden", "ocr")

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
# 渲染分辨率两档（实测依据见下）：
#
#   150 DPI：818x1317，约 93KB，约 1400 tokens —— 正文、斜体缩写、页眉都清晰，
#            但**长音符/短音符/小标点会糊成普通字母**（拉丁词典里 cŏncĭcātō 这类词
#            靠的就是这些符号区分）。
#   300 DPI：1635x2633，约 220KB，约 5700 tokens —— 长音符、短音符、重音全部可辨，
#            代价是 **token 成本 4 倍**（像素数是平方关系）。
#
# 所以不搞「一律 300」：默认 150（绝大多数文档够用），只有**小语种/多音标**这类
# 明确需要逐字符号的场景才升到 300。判据是 script_profile：非拉丁非 CJK 字母占比
# 超过阈值，或文本里出现了长音符/组合附加符号 —— 两者都指向「符号有意义」。
RENDER_DPI = 150
RENDER_DPI_DETAIL = 300
# 触发高分辨率渲染的「第三类文字」占比阈值。比 needs_vlm 的 0.30 低：
# 渲染是本地行为、不花钱，宁可多给一次高清，也不要让 Agent 看糊的字。
DETAIL_DPI_RATIO = 0.15
# 图片保留时长：超过就删。定时清理靠「每次渲染前扫一遍」实现 ——
# 桥接进程没有常驻定时器（它只在工具调用时活着），用调用点触发既简单又够用。
PAGE_PIC_TTL_SECONDS = 24 * 3600

# 长音符 / 短音符 / 组合附加符号 / 希腊文重音区。这些字符的存在本身就是
# 「这份文档靠符号区分字义」的证据，与语种无关（拉丁词典也用它们）。
#
# ⚠ 必须包含 Latin Extended-A/B（U+0100–U+024F）：`ĭ`(U+012D)、`ā`(U+0101)、
# `ŏ`(U+014F) 这些**预组合**字符都在那儿，而它们正是拉丁词典区分长短音的主力。
# 第一版只写了组合附加符号区（U+0300+），于是 `concĭcātrīzo` 被漏判 —— 自测当场抓到。
_DIACRITIC_RE = re.compile(
    "["
    "\u0100-\u017f"   # Latin Extended-A（ĭ ā ŏ ū ē …，预组合长短音符）
    "\u0180-\u024f"   # Latin Extended-B
    "\u0300-\u036f"   # Combining Diacritical Marks（组合附加符号）
    "\u02b0-\u02ff"   # 修饰字母、间距附加符号
    "\u1d00-\u1dbf"   # 小型大写/修饰字母
    "\u1e00-\u1eff"   # 拉丁扩展附加（ḗ ṑ ṝ …）
    "\u1f00-\u1fff"   # 希腊文扩展（多音标希腊文的全部重音/送气符号）
    "\u0370-\u03ff"   # 希腊文与科普特文
    "\u0400-\u04ff"   # 西里尔文
    "\u0590-\u05ff"   # 希伯来文
    "\u0600-\u06ff"   # 阿拉伯文
    "\u0900-\u097f"   # 天城文
    "]"
)


@dataclass(frozen=True)
class PdfProbe:
    """PDF 侦察结果（只读，不落盘）。

    `sample_text` 是抽样页的文本片段（截断到几千字符），只用于**判断书写系统**，
    不参与内容交付。为什么要带上它：文本层「干净」只说明字多，不说明字对 ——
    希腊文/西里尔文这类文档的文本层可能又长又整齐，但符号在提取时就丢了
    （实测：test.pdf 的 `concĭcātrīzo` 抽出来是 `concicatrizo`，长音符全没）。
    """

    pages: int
    avg_chars: float
    images: int
    tables: int
    sample_text: str = ""
    # 「文本层不可信」两个信号（2026-11 路由修复）。**必须给默认值 False**：
    # 自测与旧调用点的四参 PdfProbe(...) 构造不受影响，probe 拿不到 chars 时也置 False。
    vertical: bool = False   # 竖排：字符按列堆叠（distinct_x 少、distinct_y 多）
    ocr_font: bool = False   # 文本层字体是 OCR 藏字（GlyphLessFont / Hidden*OCR 等）


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
    显式档 > 图片 > 原生格式 > 非 PDF > layout > accuracy 的三段判断 > 自动判据；
    自动判据里，**「文本层不可信」信号（竖排 / OCR 藏字）压在「干净文本层 → fast」
    之前** —— 字多不等于字对，2026-11 路由修复（见 VERTICAL_RATIO 处的标定注释）。
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
        # `prefer="accuracy"` 的语义是「我要逐字准确」。判断按代价从低到高：
        #
        # ① 侦察失败 → 只能走本地（不为了猜而多跑一次）
        # ② 「文本层不可信」信号（竖排 / OCR 藏字层）→ **直接渲染**：这类文本层的
        #    坐标序本身就是错的，字再多也不能交给按顺序抽取的本地引擎
        #    （2026-11 修复，判据标定见 VERTICAL_RATIO 处的注释）。
        # ③ 文本层含大量非拉丁字母 → **即使文本层干净也渲染**：这类文档的文本层
        #    常常「字多但不对」（实测 test.pdf 的希腊文：文本层 4335 字/页很干净，
        #    但长音符在提取时就丢了）。原生文本对拉丁文可信，对多音标文字不可信，
        #    所以这里不看字数看**书写系统**。
        # ④ 其余干净文本层（真正的原生文本，如英文合同、中文公文）→ 走本地：
        #    渲染成图再让 Agent 读只会更慢更贵，还可能看错。
        if probe is None:
            return "fast", "accuracy-probe-failed"
        # 「文本层不可信」信号优先于一切字数判断：竖排 / OCR 藏字层的**坐标序**
        # 本身就是错的，字再多也不能交给按顺序抽取的本地引擎。声明了 accuracy
        # 就是要逐字准确 → 直接渲染页面图，理由用与 auto 相同的两个码。
        if probe.vertical:
            return "accuracy", "vertical-layout"
        if probe.ocr_font:
            return "accuracy", "ocr-hidden-text-layer"
        if probe.avg_chars >= SCANNED_AVG_CHARS and has_diacritics(probe.sample_text):
            return "accuracy", "accuracy-diacritic-text-layer"
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
    # 「文本层不可信」的两个信号（2026-11 修复）：命中任何一个都不许判
    # 「干净文本层 → fast」—— 那正是把竖排件与 OCR 藏字件送进本地、产出整页
    # 假表格的原判据漏洞。宁可白花一次云端上传，不可交一份乱序表格。
    if probe.vertical:
        return "structural", "vertical-layout"
    if probe.ocr_font:
        return "structural", "ocr-hidden-text-layer"
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


def has_diacritics(text: str) -> bool:
    """文本里是否有**承载信息**的附加符号（长音符、重音、组合符号、非拉丁文字）。

    这是「该不该用 300 DPI」的第二条判据：`script_profile` 只数**字母**，
    但拉丁词典里的 cŏncĭcātō 全是拉丁字母，靠的是附加符号区分 —— 只看字母
    会把它判成「普通拉丁文」，于是给一张糊掉长音符的图。这里直接找符号本身。
    """
    return bool(_DIACRITIC_RE.search(text or ""))


def pick_dpi(*, text: str = "", prefer: str = "auto", scanned: bool = False) -> int:
    """按内容选渲染分辨率，返回 150 或 300。

    ⚠ **文本层是不可靠的信号**（实测教训）：`test.pdf` 那份拉丁词典，纸面上满页
    `concĭcātrīzo` 的长音符，但 pdfplumber 抽出的文本层是 `concicatrizo` ——
    **符号在提取时就已经丢了**。所以「文本里有附加符号 → 300」只能当加分项，
    不能当唯一判据；真正可靠的是下面三条：

      ① 显式 `prefer="accuracy"` → **永远 300**：声明了要逐字准确，就不该省；
      ② 文本里确实有附加符号 / 第三类文字占比 > 15% → 300（能判出来就判）；
      ③ 其余 → 150。

    `scanned` 目前不参与判定，保留参数是为了将来若要「扫描件一律高清」时有落点。
    """
    if prefer == "accuracy":
        return RENDER_DPI_DETAIL
    if has_diacritics(text):
        return RENDER_DPI_DETAIL
    if other_ratio(text) > DETAIL_DPI_RATIO:
        return RENDER_DPI_DETAIL
    return RENDER_DPI


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


def render_pages(
    src: Path, *, max_pages: int = MAX_RENDER_PAGES, dpi: int = RENDER_DPI
) -> list[str]:
    """把文档的页面渲染成 PNG，返回绝对路径列表（按页序）。

    支持两类输入：
      · **PDF** —— 用 pdfplumber 逐页 `to_image()`（已实测可用；pypdfium2 也在，但
        pdfplumber 是既有依赖，不再引入第二条渲染路径）；
      · **独立图片** —— 原样复制一份到 page-pics（Agent 可以直接读原文件，
        但复制过来能让「这次的产物」集中在一处，且同样受 TTL 清理约束）。

    **`dpi` 对独立图片无效**：图片是按原样复制的，不重采样 —— 源图有多少像素就是
    多少像素，放大不会凭空长出细节。只有 PDF 需要「栅格化」，也才有 DPI 可言。

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


def _column_ratio(chars: list) -> "float | None":
    """一页字符的 distinct_y / distinct_x（坐标按 ~2pt 归并，消亚点级抖动）。

    竖排时字符按列堆叠：每列共享同一个 x0 → distinct_x（列数）少、
    distinct_y（每列字数）多，比值极高；横排相反（字符 x0 铺满页宽）。
    字符太少或 distinct_x 退化时返回 None —— 样本不足以支撑「列结构」
    的判断时宁可放过（走错本地才是大代价，但拿噪声当证据同样危险）。
    """
    if len(chars) < VERTICAL_MIN_PAGE_CHARS:
        return None
    xs = {round((c.get("x0") or 0) / 2) for c in chars}
    ys = {round((c.get("top") or 0) / 2) for c in chars}
    if len(xs) < VERTICAL_MIN_COLUMNS:
        return None
    return len(ys) / len(xs)


def _ocr_font_hit(chars: list) -> bool:
    """这页字符里是否出现过 OCR 藏字体的名字（见 OCR_FONT_MARKERS）。

    子串匹配是有意为之：藏字体的命名没有统一规范，实测见过的三种
    （GlyphLessFont / HiddenHorzOCR / HiddenVertOCR）分别命中三个片段。
    代价是理论上可能误伤名为 *OCR* 的真实字体（如 OCR-B），但误伤的代价
    只是「多走一次云端 OXR」（structural 本来就更适合它），漏放的代价是
    整页假表格 —— 两害相权取其轻。
    """
    for c in chars:
        name = (c.get("fontname") or "").lower()
        if any(marker in name for marker in OCR_FONT_MARKERS):
            return True
    return False


def probe_pdf(path: Path) -> "PdfProbe | None":
    """抽样侦察一个 PDF：页数、前 3 页均字数、图片数、前 2 页表格数，
    以及「文本层不可信」两个信号（竖排 / OCR 藏字层，见 VERTICAL_RATIO 处注释）。

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
            # 「文本层不可信」两个信号：与 texts 同一批抽样页（前 3 页）。
            # 拿不到 chars 就保持 False —— 新信号只加分不减分，绝不抛错、
            # 绝不影响既有判据（probe 失败仍走优雅退化为 None）。
            vertical = False
            ocr_font = False
            for p in sample:
                try:
                    chars = p.chars or []
                except Exception:  # noqa: BLE001 — 单页字符表坏了不影响其它页
                    continue
                if not chars:
                    continue
                ratio = _column_ratio(chars)
                if ratio is not None and ratio >= VERTICAL_RATIO:
                    vertical = True
                if _ocr_font_hit(chars):
                    ocr_font = True
    except Exception as exc:  # noqa: BLE001 — 加密 / 损坏 / 根本不是 PDF
        diag("probe.fail", f"{type(exc).__name__}: {str(exc)[:160]}")
        return None
    return PdfProbe(
        pages=pages,
        avg_chars=sum(map(len, texts)) / max(1, len(texts)),
        images=images,
        tables=tables,
        # 只留前 4000 字符：判断书写系统够用了，再多只是白占内存
        sample_text="\n".join(texts)[:4000],
        vertical=vertical,
        ocr_font=ocr_font,
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


def _forced_fast_warning(probe: "PdfProbe | None") -> str:
    """prefer="fast" 命中「文本层不可信」信号时的风险说明；无信号返回空串。

    prefer="fast" 是显式「只用本地」覆盖：**保留语义**（判据层照常返回 fast），
    但不得**静默**交出可能乱序的假表格 —— 把风险写成 note 讲明白。
    刻意不按失败处理：声明了 fast 的用户往往就是不能/不想上传（离线、隐私），
    硬失败等于把唯一可用的路也关掉；而「空输出」那种毫无用处的结果，
    另由 `_run_fast` 的护栏按失败处理。
    """
    if probe is None:
        return ""
    flags = []
    if probe.vertical:
        flags.append("竖排版面")
    if probe.ocr_font:
        flags.append("OCR 隐藏文本层")
    if not flags:
        return ""
    return (
        f"该 PDF 的文本层不可信（{'、'.join(flags)}），本地输出可能整页乱序或混入假表格；"
        "你声明了 prefer=\"fast\" 已照办走本地，需要正确版面请改用 prefer=\"layout\""
    )


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
        + (f" vertical={probe.vertical} ocr_font={probe.ocr_font}" if probe else "")
    )
    diag("route.decide", detail)

    # prefer="fast" 的非静默护栏：命中不可信文本层时把风险写进 notes（见
    # _forced_fast_warning 的说明），kind/reason 不变 —— 判据层的语义保持纯粹。
    if kind == "fast" and prefer == "fast":
        warning = _forced_fast_warning(probe)
        if warning:
            out.notes.append(warning)
            diag("route.fast.untrusted-text-layer", detail)

    if kind == "fast":
        return _run_fast(out, src, local_convert)
    if kind == "structural":
        return _run_structural(out, src, local_convert, prefer=prefer)
    return _run_accuracy(out, src, local_convert)


# ---- 路线一：本地 ----


def _run_fast(out: RouteOutcome, src: Path, local_convert) -> RouteOutcome:
    try:
        text = local_convert(src)
    except Exception as exc:  # noqa: BLE001 — 本地自己报错就是最终错误
        out.error = f"{exc}" if isinstance(exc, ToolError) else f"{type(exc).__name__}: {exc}"
        return out
    if not (text or "").strip():
        # 「成功但 0 字符」必须当失败（bake-off 实证）：markitdown 对扫描件/图片
        # 返回 status=ok + 空正文，当成功返回等于骗下游「这份文档是空的」，
        # 后面所有推理都建立在假事实上。
        out.ok = False
        out.error = (
            "本地引擎没有提取到任何内容（空输出）：文件很可能是扫描件/纯图片 PDF，"
            "或确实没有可提取的文本。请改用 prefer=\"layout\"（云端版面模型）"
            "或 prefer=\"accuracy\"（渲染页面图自己读）。"
        )
        return out
    out.ok = True
    out.engine = "markitdown"
    out.text = text
    return out


# ---- 路线二：structural（OXR；体检不过则附页面图给 Agent 校对） ----


def _run_structural(
    out: RouteOutcome, src: Path, local_convert, prefer: str = "auto"
) -> RouteOutcome:
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
    _maybe_escalate(out, src, prefer=prefer)
    if not out.page_images:
        # 没有附原图时，**必须**告诉 Agent 这是 OCR 草稿、什么时候该去核对。
        # 这条提示来自真机事故（2026-11-01）：一份竖排繁体扫描件走 layout 出了通顺
        # 正文，但里面 `笛卡尼`（应笛卡兒）、`拿破崙二世`（应一世）、`大膨刻家`
        # （应大彫刻家）、`軍家`（应軍事家）四处错字 —— 其中「軍家」是**漏字**，
        # 语法上仍然通顺，只有对照原图才能发现。
        #
        # 关键认识：**通顺 ≠ 正确**。OCR 的错误集中在三类，恰好都是不能靠常识纠正的：
        #   · 专名/译名（台湾与大陆译名差异大，模型「顺手纠正」反而会改错）；
        #   · 漏字（`軍事家`→`軍家`，句子照样读得通，最难发现）；
        #   · 形近字（`彫`/`膨`、`兒`/`尼`、`一`/`二`，数字与日期尤其危险）。
        out.notes.append(
            "OCR 结果仅供草稿：专名、译名、数字、日期、以及看起来「读得通但可疑」的字，"
            "请传 prefer=\"accuracy\" 用原图核对 —— 通顺不等于正确，漏字与形近字"
            "（如「軍事家」→「軍家」）在文本上依然通顺，只有对照原图才能发现"
        )
        # 尾部保守提示（bake-off 实证）：OXR 会幻觉整句收尾并丢末条 —— Souter 页尾
        # 多出一句页面里没有的英文、末条 Ǎbrăhǎm 被丢掉。可靠的尾部自动检测需要
        # 视觉对比原图，第一步刻意不做（不可靠的启发式会误删正文），只把风险讲清楚。
        out.notes.append(
            "云端结果的结尾可能不可靠：实测会幻觉出原文没有的收尾句、或丢掉最后一条；"
            "需要引用原文（尤其中文/日文）时，请对照页面末尾核对"
        )
    return out


def _maybe_escalate(out: RouteOutcome, src: Path, prefer: str = "auto") -> None:
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
    # 分辨率按**已拿到的文本 + 用户声明的档位**挑：OXR 结果里有附加符号
    # （希腊文重音、拉丁长音符）说明这些符号承载信息，必须 300 DPI 才看得清。
    dpi = pick_dpi(text=out.text, prefer=prefer)
    images = render_pages(src, dpi=dpi)
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
        f"已附上 {len(images)} 张页面图（{dpi} DPI）—— 请用 read_image 自己核对"
    )


# ---- 路线三：accuracy（Agent 亲自看） ----


def _run_accuracy(out: RouteOutcome, src: Path, local_convert) -> RouteOutcome:
    """`prefer="accuracy"`：把页面渲染成图，交给 **Agent 自己**读。

    这条路**不受云端开关影响**（`DSH_OXR_MODE=off` 关的是上传，不是 Agent 的眼睛），
    也不需要任何 key —— 渲染在本机完成，Agent 用 `read_image` 读本地文件。

    与旧实现的关键差别：这里**不产生文本**。页面图的像素交给 Agent，
    由 Agent 自己转写 —— 少一道有损转换，精度上限就是 Agent 自己的视觉能力。

    **分辨率固定 300 DPI**：走到这条路的都是「声明要逐字准确」的场景
    （多音标小语种、手写、极小字号），这时省 token 就是省错了地方。
    实测 150 DPI 会把长音符糊成普通字母，而 300 DPI 全部可辨。
    """
    dpi = RENDER_DPI_DETAIL
    images = render_pages(src, dpi=dpi)
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
        f"已把 {len(images)} 张页面图（{dpi} DPI）交给你自己读：请用 read_image 逐张查看，"
        "按页序拼接为 Markdown。这条路不经任何 OCR，精度取决于你自己的视觉"
    )
    # 逐字转写时最常错的四类，写在 note 里当核对清单 —— 它们都属于
    # 「看起来没错、但一对照原图就露馅」的类型（真机事故见 _run_structural 的注释）。
    out.notes.append(
        "转写时请特别核对：专名与译名（勿按常识「纠正」，台湾/大陆译名差异很大）、"
        "漏字（句子通顺但少了字，最难发现）、形近字（彫/膨、兒/尼）、"
        "以及数字与日期"
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
    # 干净文本层但含多音标希腊文 → 即使字多也要渲染（文本层「字多但不对」）
    greek_layer = PdfProbe(
        pages=3,
        avg_chars=4335,
        images=0,
        tables=0,
        sample_text="1 Περὶ πολλῶν ἂν ποιησαίμην, ὦ ἄνδρες, τοῦτο ὑμᾶς ἐμοὶ δικαστὰς",
    )
    check("accuracy+希腊文文本层", decide(".pdf", greek_layer, "accuracy"), "accuracy")
    check(
        "理由 希腊文文本层",
        decide_route(".pdf", greek_layer, "accuracy")[1],
        "accuracy-diacritic-text-layer",
    )
    # 但 auto 档不受影响：文本层干净就还是本地（不主动多花 token）
    check("auto+希腊文文本层", decide(".pdf", greek_layer, "auto"), "fast")
    # 普通英文/中文的干净文本层仍走本地（回归用例不能丢）
    en_layer = PdfProbe(pages=3, avg_chars=2000, images=0, tables=0,
                        sample_text="This Agreement is made and entered into as of the date")
    check("accuracy+英文文本层", decide(".pdf", en_layer, "accuracy"), "fast")
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

    # —— 「文本层不可信」两个信号（2026-11 路由修复）——
    # 竖排件与 OCR 藏字件都有文本层、字都不少，原判据把它们送进了 fast；
    # 现在命中任何一个信号都必须降级，禁走 fast。
    vertical = PdfProbe(pages=2, avg_chars=1155, images=2, tables=0, vertical=True)
    souter = PdfProbe(pages=1, avg_chars=3881, images=2, tables=0, ocr_font=True)
    check("auto+竖排", decide(".pdf", vertical, "auto"), "structural")
    check("理由 auto+竖排", decide_route(".pdf", vertical, "auto")[1], "vertical-layout")
    check("auto+OCR 藏字", decide(".pdf", souter, "auto"), "structural")
    check("理由 auto+OCR 藏字", decide_route(".pdf", souter, "auto")[1], "ocr-hidden-text-layer")
    check("accuracy+竖排", decide(".pdf", vertical, "accuracy"), "accuracy")
    check("理由 accuracy+竖排", decide_route(".pdf", vertical, "accuracy")[1], "vertical-layout")
    check("accuracy+OCR 藏字", decide(".pdf", souter, "accuracy"), "accuracy")
    check("理由 accuracy+OCR 藏字", decide_route(".pdf", souter, "accuracy")[1], "ocr-hidden-text-layer")
    # 新字段有默认值：不给信号的旧构造，行为一个都不能变（回归）
    check("默认字段 干净文本层仍 fast", decide(".pdf", clean, "auto"), "fast")
    check("默认字段 表格仍 structural", decide(".pdf", table, "auto"), "structural")
    # prefer="fast" 显式覆盖保留：判据层照常返回 fast（风险说明在编排层写成 note）
    check("prefer=fast 覆盖竖排", decide(".pdf", vertical, "fast"), "fast")
    # 编排层纯函数：强制 fast 命中信号时必须给出非空风险说明，无信号时不打扰
    check_true("强制fast 竖排有提示", bool(_forced_fast_warning(vertical)))
    check_true("强制fast OCR 藏字有提示", bool(_forced_fast_warning(souter)))
    check_true("强制fast 提示写明 layout", "layout" in _forced_fast_warning(souter))
    check("强制fast 干净文本层无提示", _forced_fast_warning(clean), "")
    check("强制fast 无侦察", _forced_fast_warning(None), "")

    # _column_ratio：竖排比值高、横排比值低、样本不足/退化返回 None
    def _char(x, top):
        return {"x0": x, "top": top}

    # 竖排：4 列，每列 20 字，列内共享 x0
    vert_chars = [_char(100.0 + (i // 20) * 30.0, 50.0 + (i % 20) * 12.0) for i in range(80)]
    check_true("_column_ratio 竖排命中阈值", _column_ratio(vert_chars) >= VERTICAL_RATIO)
    # 横排：字符 x0 铺满页宽，distinct_y 只有几行
    horiz_chars = [_char(50.0 + (i % 60) * 9.0, 100.0 + (i // 60) * 14.0) for i in range(120)]
    check_true("_column_ratio 横排低于阈值", _column_ratio(horiz_chars) < VERTICAL_RATIO)
    check("_column_ratio 字符太少", _column_ratio([_char(1.0, 1.0)]), None)
    single_col = [_char(100.0, 50.0 + i * 12.0) for i in range(60)]
    check("_column_ratio 单列退化", _column_ratio(single_col), None)
    # _ocr_font_hit：藏字字体命中、真实排版字体不命中
    check_true("_ocr_font_hit GlyphLessFont", _ocr_font_hit([_char(1, 1), {"x0": 2, "top": 2, "fontname": "GlyphLessFont"}]))
    check_true("_ocr_font_hit HiddenVertOCR", _ocr_font_hit([{"x0": 1, "top": 1, "fontname": "HiddenVertOCR"}]))
    check("_ocr_font_hit 真实字体", _ocr_font_hit([{"x0": 1, "top": 1, "fontname": "MBaskerville"}, {"x0": 2, "top": 2, "fontname": "Noto-Sans-SC"}]), False)
    check("_ocr_font_hit 空字符表", _ocr_font_hit([]), False)

    # fast 路空输出护栏（bake-off 实证：markitdown 对扫描件/图片返回 ok + 0 字符）
    res_empty = _run_fast(RouteOutcome(ok=False, kind="fast"), Path("_stub_.pdf"), lambda p: "")
    check("fast 空输出当失败", res_empty.ok, False)
    check_true("fast 空输出提示扫描件", "扫描件" in (res_empty.error or ""))
    check("fast 纯空白也当失败",
          _run_fast(RouteOutcome(ok=False, kind="fast"), Path("_stub_.pdf"), lambda p: "  \n\t ").ok,
          False)
    res_ok = _run_fast(RouteOutcome(ok=False, kind="fast"), Path("_stub_.pdf"), lambda p: "有内容")
    check("fast 有内容仍成功", res_ok.ok, True)
    check("fast 有内容引擎", res_ok.engine, "markitdown")

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

    # has_diacritics / pick_dpi：分辨率选择（实测依据见常量注释）
    check_true("has_diacritics(长音符)", has_diacritics("concĭcātrīzo"))
    check_true("has_diacritics(组合符号)", has_diacritics("concibo\u0304"))
    check_true("has_diacritics(希腊文)", has_diacritics("Πρόκειται τῇδε"))
    check("has_diacritics(纯英文)", has_diacritics("comrade-in-arms (C.I.L. 8. 9060)"), False)
    check("has_diacritics(纯中文)", has_diacritics("物業收回通知書 請於七日內繳清欠款"), False)
    check("has_diacritics(空串)", has_diacritics(""), False)

    check("pick_dpi(默认)", pick_dpi(), RENDER_DPI)
    check("pick_dpi(纯英文)", pick_dpi(text="comrade-in-arms"), RENDER_DPI)
    check("pick_dpi(长音符)", pick_dpi(text="concĭcātrīzo, make to scar"), RENDER_DPI_DETAIL)
    check("pick_dpi(希腊文)", pick_dpi(text=greek), RENDER_DPI_DETAIL)
    # 显式 accuracy 永远 300：已经声明要逐字准确，就不该为省 token 降档
    check("pick_dpi(accuracy 覆盖)", pick_dpi(text="comrade-in-arms", prefer="accuracy"), RENDER_DPI_DETAIL)
    check_true("高分辨率确实更高", RENDER_DPI_DETAIL > RENDER_DPI)
    check_true("高分辨率是默认的 2 倍", RENDER_DPI_DETAIL == RENDER_DPI * 2)

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

    # 兄弟模块的纯函数（同样是断网可跑的自测；markitdown_tools 只测纯函数，
    # 不会触发 import markitdown / numpy —— import 发生在 _converter() 里）
    for module_name in ("oxr_client", "markitdown_tools"):
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
    print("OK   doc_route / oxr_client / markitdown_tools 纯函数自测全部通过（含页面图 TTL 清理）")
    return 0


if __name__ == "__main__":
    import sys

    if "--self-test" in sys.argv[1:]:
        raise SystemExit(_self_test())
    print("用法：python doc_route.py --self-test（本模块不注册工具，由 markitdown_tools 调用）")
    raise SystemExit(2)
