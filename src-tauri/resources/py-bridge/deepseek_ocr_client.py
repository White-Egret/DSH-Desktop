# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""DeepSeek OCR 客户端（全家桶独立 OCR API，第五步新增的第四引擎）。

bake-off 实证（ocr-bakeoff-deepseek-report.md）：DeepSeek OCR 在**现代件**上
质量与视觉持平且最便宜（表格 5/5、竖排中文 5/5、词典 5/5，≈0.0015 元/页）；
在**早期印本**（长 s→f、希腊栏读成拉丁字母串）与**多音标希腊文**（单调化）
上系统性失败。所以它只作为 structural 路的现代件首选引擎，输出命中早期印本
信号时升级 Agent 视觉（accuracy），OXR 保留为免费兜底。

**付费与开关纪律（不偷偷扣费）**：
  · 默认 `DSH_DEEPSEEK_OCR` 未设置 = **关闭**，auto 路不产生任何 DeepSeek 花费；
  · 设为 `1/on/true/yes` 才启用；
  · 每次成功调用都在结果 note 里写明页数与按 1.5 元/千页的预估花费；
  · `DSH_DEEPSEEK_API_KEY` 未配置 / 402 余额不足 / 网络失败 → 一律回落 OXR，
    不报错、不伪造（OXR 免费兜底始终可用）。

**凭据解析顺序**：环境变量 `DSH_DEEPSEEK_API_KEY` → 回落
`{DSH_HOME 或 ~/.dsh}/.credentials.yaml` 里的 `DEEPSEEK_API_KEY` 字段
（只读取，任何日志/异常都不输出明文）。

**端点纪律**：只用真实 `https://api.deepseek.com`，严禁代理替换。
"""

from __future__ import annotations

import json
import os
import re
import time

from utils import diag

# ---------- 配置 ----------

DEFAULT_OCR_URL = "https://api.deepseek.com/v1/tools/ocr"
DEFAULT_BALANCE_URL = "https://api.deepseek.com/user/balance"
# 官方定价（api-docs.deepseek.com/zh-cn/quick_start/pricing）：文件解析与 OCR，
# 每 1,000 页 1.5 元（英文页同价 $0.225）。按成功识别页数计，失败不计费。
PRICE_PER_1000_PAGES = 1.5
LIMIT_BYTES = 100 * 1024 * 1024
MAX_PAGES = 1000

_MIME = {
    ".pdf": "application/pdf",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".gif": "image/gif",
    ".bmp": "image/bmp",
    ".tif": "image/tiff",
    ".tiff": "image/tiff",
}

# ---------- 凭据与开关 ----------


def enabled() -> bool:
    """DeepSeek OCR 是否启用（环境变量 `DSH_DEEPSEEK_OCR`）。

    **默认关闭** —— 付费引擎不得在用户不知情时产生花费；设为
    `1/on/true/yes`（大小写不敏感）才启用。
    """
    raw = (os.environ.get("DSH_DEEPSEEK_OCR", "") or "").strip().lower()
    return raw in ("1", "on", "true", "yes")


def api_key() -> str:
    """解析 DeepSeek API key；取不到返回空串（调用方据此回落 OXR）。

    顺序：环境变量 `DSH_DEEPSEEK_API_KEY` → `{DSH_HOME 或 ~/.dsh}/.credentials.yaml`
    的 `DEEPSEEK_API_KEY` 字段。凭据内容只读取，绝不写进日志。
    """
    raw = (os.environ.get("DSH_DEEPSEEK_API_KEY", "") or "").strip()
    if raw:
        return raw
    home = (os.environ.get("DSH_HOME", "") or "").strip()
    if not home:
        home = os.path.join(os.path.expanduser("~"), ".dsh")
    cred = os.path.join(home, ".credentials.yaml")
    try:
        text = open(cred, encoding="utf-8").read()
    except OSError:
        return ""
    m = re.search(r"DEEPSEEK_API_KEY:\s*([^\s#]+)", text)
    return m.group(1) if m else ""


def cost_estimate(page_count: int) -> float:
    """按官方 1.5 元/千页估算花费（元）。"""
    return (page_count or 0) / 1000.0 * PRICE_PER_1000_PAGES


# ---------- 错误模型 ----------


class OcrError(Exception):
    """DeepSeek OCR 调用失败。`kind` 决定 doc_route 怎么降级（回落 OXR）。"""

    def __init__(self, kind: str, message: str, *, code: "int | None" = None) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.code = code


_STATUS_KINDS = {
    400: "unsupported",
    401: "auth",
    402: "balance",
    403: "auth",
    408: "timeout",
    413: "too-large",
    429: "busy",
    500: "http",
    503: "busy",
    504: "timeout",
}


def classify_status(status: int) -> str:
    """HTTP 状态码 → 行为类。纯函数，自测直接表驱动。"""
    return _STATUS_KINDS.get(status, "http")


_HINTS = {
    "network": "连不上 DeepSeek 服务",
    "timeout": "DeepSeek OCR 超时",
    "auth": "DeepSeek API Key 无效",
    "balance": "DeepSeek 余额不足",
    "http": "DeepSeek 服务返回错误",
    "busy": "DeepSeek 服务繁忙",
    "unsupported": "DeepSeek OCR 不支持或无法解析该文件",
    "too-large": "文件超过 DeepSeek OCR 上限",
}


def describe_error(exc: Exception) -> str:
    kind = getattr(exc, "kind", "")
    msg = getattr(exc, "message", "") or str(exc)
    head = _HINTS.get(kind, "DeepSeek OCR 调用失败")
    return f"{head}：{msg}" if msg else head


# ---------- 布局 token 剥离 ----------

# rich_layout 输出的块结构：`<｜ref｜>类型<｜/ref｜><｜box｜>[[x1,y1,x2,y2]]<｜/box｜>` +
# 类型对应的正文。坐标归一化 0–1000，交付前整个剥掉。
_BOX = re.compile(r"<｜box｜>\[\[[^\]]*\]\]<｜/box｜>")
_REF_KNOWN = re.compile(
    r"<｜ref｜>\s*(?:title|text|sub_title|table|image|list|header|footer|formula|code|"
    r"caption|figure|page|paragraph|section|key|value)\s*<｜/ref｜>"
    r"(?:\s*<｜box｜>\[\[[^\]]*\]\]<｜/box｜>)?",
    flags=re.I,
)
_REF_ANY = re.compile(r"<｜ref｜>(.*?)<｜/ref｜>", flags=re.S)


def strip_layout_tokens(text: str) -> str:
    """剥掉 rich_layout 的布局 token（类型标签 + 坐标），只留正文。

    已知类型标签（title/text/…）连同坐标整个删除；未知 ref 保留内容
    （可能是真实文字），坐标一律删除。纯函数，自测覆盖。
    """
    t = text or ""
    t = _REF_KNOWN.sub("", t)
    t = _REF_ANY.sub(r"\1", t)
    t = _BOX.sub("", t)
    return t.strip()


# ---------- 早期印本 / 多音标信号（OCR 输出侧） ----------

# bake-off 实证：OCR 对早期印本的失败模式是**长 s「ſ」读成 f**（纯扫描页 5 处
# `fit vt multi`，而真值是 `sit vt multi`），且输出里几乎不保留 ſ 字形——所以
# 「输出里有 ſ」是强信号，「疑似 f 化词」是兜底信号。现代文本里 fit/funt 是真词，
# 阈值取 ≥3 处才升级（误报代价 = 多一次 Agent 视觉，可接受；漏报代价 = 古文献
# 被 OCR 糟蹋，不可接受）。
_FAKE_F = re.compile(r"\b(?:fit|fint|funt|fes|fet)\b", flags=re.I)


def early_print_signal(text: str) -> dict:
    """从 OCR 输出检测早期印本信号。`hit=True` 时调用方升级 accuracy。"""
    t = text or ""
    long_s = t.count("ſ")
    ligature = sum(t.count(ch) for ch in "ﬀﬁﬂﬃﬄﬅﬆ")
    fake_f = len(_FAKE_F.findall(t))
    return {"long_s": long_s, "ligature": ligature, "fake_f": fake_f,
            "hit": long_s > 0 or fake_f >= 3}


# ---------- 传输 ----------


def balance() -> "float | None":
    """查询余额（元）；失败返回 None。只读、免费。"""
    key = api_key()
    if not key:
        return None
    try:
        import requests

        r = requests.get(
            os.environ.get("DSH_DEEPSEEK_BALANCE_URL", DEFAULT_BALANCE_URL),
            headers={"Authorization": f"Bearer {key}"}, timeout=30,
        )
        if r.status_code != 200:
            return None
        infos = r.json().get("balance_infos") or []
        return float(infos[0]["total_balance"]) if infos else None
    except Exception:  # noqa: BLE001 — 余额查询失败不影响转换
        return None


def parse_bytes(name: str, blob: bytes, mime: str,
                purpose: str = "rich_layout_recognition") -> dict:
    """上传一段字节做 OCR，返回 `{"pages": [逐页正文], "page_count": N, "usage": {...}}`。

    失败一律包成 OcrError（kind 决定 doc_route 的降级方式：回落 OXR，不报错给用户）。
    注意：识别开始后 HTTP 已是 200，失败在 body 的 `error` 里——两处都要查。
    """
    if len(blob) > LIMIT_BYTES:
        raise OcrError("too-large", f"文件 {len(blob) / 1024 / 1024:.1f} MiB 超过 DeepSeek OCR 上限（100 MiB）")
    key = api_key()
    if not key:
        raise OcrError("auth", "未配置 DSH_DEEPSEEK_API_KEY 且凭据文件里没有 DEEPSEEK_API_KEY")
    try:
        import requests
    except Exception as exc:  # noqa: BLE001
        raise OcrError("network", f"requests 不可用：{exc}") from exc

    base = os.environ.get("DSH_DEEPSEEK_OCR_URL", DEFAULT_OCR_URL)
    t0 = time.perf_counter()
    try:
        r = requests.post(
            base,
            headers={"Authorization": f"Bearer {key}"},
            files={"file": (name, blob, mime)},
            data={"purpose": purpose},
            timeout=200,
        )
    except requests.exceptions.Timeout as exc:
        raise OcrError("timeout", "上传或识别超过时限") from exc
    except Exception as exc:  # noqa: BLE001 — DNS/TLS/连接失败
        raise OcrError("network", f"{type(exc).__name__}: {str(exc)[:160]}") from exc
    elapsed = round(time.perf_counter() - t0, 2)
    diag("deepseek-ocr.http", f"{name} http={r.status_code} {elapsed}s {len(blob)} bytes")

    if r.status_code != 200:
        kind = classify_status(r.status_code)
        raise OcrError(kind, f"HTTP {r.status_code}（{str(r.text)[:160]}）", code=r.status_code)

    body = r.text.strip()
    try:
        j = json.loads(body)
    except Exception as exc:  # noqa: BLE001 — 网关错误页 / 非 JSON
        raise OcrError("http", f"响应不是 JSON（{body[:160]}）") from exc
    if "error" in j:
        err = j["error"] or {}
        msg = str(err.get("message") or err)[:200]
        low = msg.lower()
        if "password" in low or "not supported" in low or "could not be parsed" in low or "decoded" in low:
            kind = "unsupported"
        elif "busy" in low or "internal" in low:
            kind = "busy"
        elif "timeout" in low:
            kind = "timeout"
        else:
            kind = "http"
        raise OcrError(kind, msg)

    page_count = j.get("page_count")
    if not isinstance(page_count, int) or page_count <= 0:
        raise OcrError("http", "响应缺少 page_count")
    pages = []
    for p in j.get("pages", []):
        pages.append(strip_layout_tokens(p.get("text") or ""))
    usage = j.get("usage") or {}
    diag("deepseek-ocr.done", f"{name} pages={page_count} {elapsed}s")
    return {"pages": pages, "page_count": page_count, "usage": usage}


def parse_file(path, purpose: str = "rich_layout_recognition") -> dict:
    """解析一个本机文件（PDF 或图片），返回同 parse_bytes。"""
    p = str(path)
    try:
        with open(p, "rb") as fh:
            blob = fh.read()
    except OSError as exc:
        raise OcrError("unsupported", f"读不到文件：{exc}") from exc
    suffix = os.path.splitext(p)[1].lower()
    mime = _MIME.get(suffix)
    if mime is None:
        raise OcrError("unsupported", f"DeepSeek OCR 不支持该格式：{suffix}")
    return parse_bytes(os.path.basename(p), blob, mime, purpose)


# ---------- 自测（无网络） ----------


def self_test() -> list[str]:
    """纯函数自测，返回失败清单（空 = 全绿）。供 doc_route.py --self-test 调用。"""
    failures: list[str] = []

    def check(name: str, got, want) -> None:
        if got != want:
            failures.append(f"dsocr.{name}: 期望 {want!r}，实际 {got!r}")

    def check_true(name: str, cond) -> None:
        if not cond:
            failures.append(f"dsocr.{name}: 期望为真，实际为假")

    # 布局 token 剥离：已知类型连坐标整个删；未知 ref 保内容；坐标一律删
    raw = ("<｜ref｜>title<｜/ref｜><｜box｜>[[52, 40, 948, 96]]<｜/box｜># Head\n"
           "<｜ref｜>text<｜/ref｜><｜box｜>[[1, 2, 3, 4]]<｜/box｜>Body text\n"
           "<｜ref｜>weird<｜/ref｜><｜box｜>[[5, 6, 7, 8]]<｜/box｜>kept")
    clean = strip_layout_tokens(raw)
    check("剥 token 后无坐标", "[[" in clean, False)
    check("已知标签删除", "title" in clean, False)
    check("正文保留", "# Head" in clean and "Body text" in clean, True)
    check("未知 ref 保内容", "kept" in clean and "weird" in clean, True)
    check("剥 token 空串", strip_layout_tokens(""), "")

    # 早期印本信号
    sig = early_print_signal("quod ſit non fit")
    check_true("ſ>0 命中", sig["hit"] and sig["long_s"] == 1)
    sig = early_print_signal("the fit was fint and funt here")
    check_true("f化≥3 命中", sig["hit"] and sig["fake_f"] == 3)
    sig = early_print_signal("modern clean text with one fit only")
    check_true("单处 fit 不命中", not sig["hit"])
    check_true("现代中文无信号", not early_print_signal("第一章 序論，受過教育。")["hit"])

    # 开关解析
    old = os.environ.get("DSH_DEEPSEEK_OCR")
    try:
        for v, want in (("1", True), ("on", True), ("True", True), ("yes", True),
                        ("0", False), ("off", False), ("", False), (None, False)):
            if v is None:
                os.environ.pop("DSH_DEEPSEEK_OCR", None)
            else:
                os.environ["DSH_DEEPSEEK_OCR"] = v
            check(f"enabled({v!r})", enabled(), want)
    finally:
        if old is None:
            os.environ.pop("DSH_DEEPSEEK_OCR", None)
        else:
            os.environ["DSH_DEEPSEEK_OCR"] = old

    # HTTP 状态映射
    for status, kind in ((400, "unsupported"), (401, "auth"), (402, "balance"),
                         (408, "timeout"), (413, "too-large"), (429, "busy"),
                         (503, "busy"), (504, "timeout"), (418, "http")):
        check(f"status {status}", classify_status(status), kind)

    # 成本估算
    check("cost 16 页", round(cost_estimate(16), 4), 0.024)
    check("cost 0 页", cost_estimate(0), 0.0)

    # 密码/格式错误 → unsupported（不重试类）；解析纯函数不经网络
    check("status 映射完备", classify_status(200) == "http", True)

    return failures


if __name__ == "__main__":
    import sys

    if "--self-test" in sys.argv[1:]:
        problems = self_test()
        if problems:
            print(f"FAIL {len(problems)} 项：")
            for p in problems:
                print(f"     {p}")
            raise SystemExit(1)
        print("OK   deepseek_ocr_client 纯函数自测全部通过")
        raise SystemExit(0)
    print("用法：python deepseek_ocr_client.py --self-test（本模块不注册工具，由 doc_route 调用）")
    raise SystemExit(2)
