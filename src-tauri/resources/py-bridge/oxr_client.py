# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""OXR 云端版面模型客户端（SoMark 免费接口，同步解析）。

契约是**实调验证**过的，不是照文档猜的（方案 §1.4）：

```text
POST {BASE}/parse/sync   (multipart/form-data)
  file=@x.pdf|@x.jpg   api_key=dummy   output_formats=["markdown"]
  element_formats={"table":"markdown","formula":"latex","image":"none","cs":"image"}
→ 200 {"code":0,"message":"解析成功",
        "data":{"task_id":…,"metadata":{"page_num":3,"file_type":".pdf"},
                "result":{"file_name":…,"outputs":{"markdown":"# …"}}}}
```

三个实调细节（都写进了代码，别当成可选项）：

1. **只请求 `markdown`**：`json` 带每块 bbox，体积十几倍；
2. **`image="none"` 实测未抑制图片链接**：必须由 `doc_route.postprocess` 后处理剥掉；
3. **`code != 0` 一律按错误处理** —— 免费接口连 `api_key` 都不校验，但不代表它不会报错。

错误码 → 行为按方案 §7.2 的表逐条实现。三个「**不重试**」的码（`1123` 额度、
`1108` 无订单、`1107` key 无效）是这张表的重点：它们重试一万次也不会成功，
而重试会让用户白等 —— 更糟的是把「欠费」显示成「服务异常」，让人去翻配置。
"""

from __future__ import annotations

import io
import json
import os
import time

from utils import diag

# ---------- 配置（全部走环境变量，v1 不做设置页开关，见方案 §12） ----------

DEFAULT_BASE = "https://free-oxr.somark.cn/api/v1"
DEFAULT_KEY = "dummy"
LIMIT_PAGES = 20
LIMIT_BYTES = 10 * 1024 * 1024
# >20 页时按 20 页切片，最多 3 片（60 页）；再长就明确拒绝并建议自部署/正式版（方案 §8 决定一）
MAX_CHUNKS = 3
MAX_TOTAL_PAGES = LIMIT_PAGES * MAX_CHUNKS

_MIME = {
    ".pdf": "application/pdf",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".bmp": "image/bmp",
    ".tif": "image/tiff",
    ".tiff": "image/tiff",
}

# 错误码 → 分类。分类决定「重试 / 不重试 / 分片 / 兜底」，不是给人看的文案表。
RETRYABLE_CODES = frozenset({1101, 1124})       # 过频 / QPS 超限：退避后重试一次
TIMEOUT_CODES = frozenset({1102})               # 任务超时：直接兜底，建议稍后重试
UNSUPPORTED_CODES = frozenset({1104, 1106, 1137})  # 不支持 / 损坏 / 解析失败
TOO_LARGE_CODES = frozenset({1118, 1132})       # 超大小 / 超页数：未分片则分片
QUOTA_CODES = frozenset({1123})                 # 套餐剩余页数不足
NO_ORDER_CODES = frozenset({1108})              # 没有有效订单
BAD_KEY_CODES = frozenset({1107})               # 无效的 API Key

# 这几个码**永远不重试**（方案 §7.2 / §10：1123、1108、1107 都不进重试分支）
NEVER_RETRY_CODES = QUOTA_CODES | NO_ORDER_CODES | BAD_KEY_CODES


class OxrError(Exception):
    """OXR 调用失败。`kind` 决定 doc_route 怎么降级，`message` 是给人看的那句话。"""

    def __init__(self, kind: str, message: str, *, code: "int | None" = None) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message
        self.code = code


def base_url() -> str:
    return (os.environ.get("DSH_OXR_BASE_URL", DEFAULT_BASE) or DEFAULT_BASE).rstrip("/")


def api_key() -> str:
    return os.environ.get("DSH_OXR_API_KEY", DEFAULT_KEY) or DEFAULT_KEY


def timeout() -> float:
    try:
        return float(os.environ.get("DSH_OXR_TIMEOUT", "180"))
    except ValueError:
        return 180.0


def classify_code(code: int) -> str:
    """把 OXR 的业务码归成行为类。纯函数，`--self-test` 直接表驱动它。

    → 'ok' | 'retry' | 'timeout' | 'unsupported' | 'too-large' | 'quota'
      | 'no-order' | 'auth' | 'unknown'
    """
    if code == 0:
        return "ok"
    if code in RETRYABLE_CODES:
        return "retry"
    if code in TIMEOUT_CODES:
        return "timeout"
    if code in UNSUPPORTED_CODES:
        return "unsupported"
    if code in TOO_LARGE_CODES:
        return "too-large"
    if code in QUOTA_CODES:
        return "quota"
    if code in NO_ORDER_CODES:
        return "no-order"
    if code in BAD_KEY_CODES:
        return "auth"
    return "unknown"


def should_retry(code: int) -> bool:
    """只有过频类码值得重试 —— 额度/订单/key 类的重试永远不会成功。"""
    return classify_code(code) == "retry" and code not in NEVER_RETRY_CODES


_HINTS = {
    "retry": "请求过于频繁（服务端限流）",
    "timeout": "云端解析超时，建议稍后重试",
    "unsupported": "云端不支持或无法解析该文件",
    "too-large": "文件超过云端上限",
    "quota": "云端额度已用尽（套餐剩余页数不足），需充值或换引擎",
    "no-order": "云端账号没有有效订单，请检查账号/套餐（或改用自部署的 DSH_OXR_BASE_URL）",
    "auth": "云端 API Key 无效，请检查 DSH_OXR_API_KEY",
    "unknown": "云端返回了未分类的错误",
}


def describe_code(code: int, message: str = "") -> str:
    kind = classify_code(code)
    hint = _HINTS.get(kind, _HINTS["unknown"])
    tail = f"（code {code}：{message}）" if message else f"（code {code}）"
    return f"{hint}{tail}"


# ---------- 传输 ----------


def _post(blob: bytes, name: str, mime: str) -> dict:
    """发一次同步解析请求，返回 `data` 块。所有失败都包成 OxrError。"""
    try:
        import requests
    except Exception as exc:  # noqa: BLE001 — requests 由 markitdown 带入，理论上必然有
        raise OxrError("network", f"requests 不可用：{exc}") from exc

    data = {
        "api_key": api_key(),
        "output_formats": ["markdown"],
        "element_formats": json.dumps(
            {"table": "markdown", "formula": "latex", "image": "none", "cs": "image"}
        ),
    }
    try:
        resp = requests.post(
            f"{base_url()}/parse/sync",
            data=data,
            files={"file": (name, blob, mime)},
            timeout=timeout(),
        )
    except Exception as exc:  # noqa: BLE001 — 连接失败 / 超时 / DNS / TLS
        kind = "timeout" if "timed out" in str(exc).lower() else "network"
        raise OxrError(kind, f"{type(exc).__name__}: {str(exc)[:200]}") from exc

    try:
        body = resp.json()
    except Exception as exc:  # noqa: BLE001 — 网关错误页 / HTML 错误
        raise OxrError(
            "http", f"HTTP {resp.status_code}，响应不是 JSON（{str(resp.text)[:160]}）"
        ) from exc

    if not isinstance(body, dict):
        raise OxrError("http", f"HTTP {resp.status_code}，响应结构异常")
    code = body.get("code")
    try:
        code_int = int(code)
    except (TypeError, ValueError):
        code_int = -1
    if code_int != 0:
        raise OxrError(
            classify_code(code_int),
            describe_code(code_int, str(body.get("message") or "")),
            code=code_int,
        )
    data_block = body.get("data") or {}
    if not isinstance(data_block, dict):
        raise OxrError("http", "响应缺少 data 块")
    return data_block


def extract_markdown(data_block: dict) -> str:
    """从 `data.result.outputs` 取 markdown。

    只含**你请求过的**格式（方案 §11）—— 所以这里只找 `markdown`，
    找不到就是服务端行为变了，必须报错而不是静默返回空串。
    """
    result = (data_block or {}).get("result") or {}
    outputs = (result.get("outputs") or {}) if isinstance(result, dict) else {}
    md = outputs.get("markdown")
    if md is None:
        raise OxrError("http", "响应里没有 markdown 输出（可能只返回了其它格式）")
    return str(md)


def parse_bytes(name: str, blob: bytes, mime: str = "") -> str:
    """解析一段字节。过频码退避 2 秒重试**一次**，其余失败立刻抛。"""
    if len(blob) > LIMIT_BYTES:
        raise OxrError(
            "too-large",
            f"文件 {len(blob) / 1024 / 1024:.1f} MB 超过云端 {LIMIT_BYTES // 1024 // 1024} MB 上限",
        )
    mime = mime or _MIME.get(os.path.splitext(name)[1].lower(), "application/octet-stream")
    try:
        return extract_markdown(_post(blob, name, mime))
    except OxrError as exc:
        if not should_retry(exc.code or -1):
            raise
        time.sleep(2)
        return extract_markdown(_post(blob, name, mime))


def page_count(blob: bytes) -> "int | None":
    """用 pypdf 数页数。**pypdf 缺失就返回 None**（不抛）—— 老版本基本安装里没有它，
    这时按「不知道页数」处理：能传就传，服务端嫌大再报错（方案 §8 硬约束 1）。"""
    try:
        from pypdf import PdfReader  # noqa: PLC0415 — 可选依赖，必须惰性导入
    except Exception:  # noqa: BLE001
        return None
    try:
        return len(PdfReader(io.BytesIO(blob)).pages)
    except Exception:  # noqa: BLE001 — 加密 / 损坏
        return None


def split_pdf(blob: bytes, pages_per_chunk: int = LIMIT_PAGES) -> "list[bytes]":
    """按页切片，返回各片 PDF 字节。

    pypdf 缺失时抛 OxrError('too-large')，文案里给出**可操作**的提示 ——
    只说「文件太大」会让人去压缩 PDF，而真正的动作是点一次「基本安装」。
    """
    try:
        from pypdf import PdfReader, PdfWriter  # noqa: PLC0415
    except Exception as exc:  # noqa: BLE001
        raise OxrError(
            "too-large",
            "这份 PDF 超过云端单次上限，需要按页切片，但本机缺少 pypdf。"
            "请在首选项「Python 环境」里点一次「基本安装」（会补装 pypdf），然后重试。",
        ) from exc
    try:
        reader = PdfReader(io.BytesIO(blob))
        total = len(reader.pages)
    except Exception as exc:  # noqa: BLE001
        raise OxrError("unsupported", f"读取 PDF 页失败：{type(exc).__name__}: {exc}") from exc
    if total > MAX_TOTAL_PAGES:
        raise OxrError(
            "too-large",
            f"这份 PDF 有 {total} 页，超过云端 {MAX_TOTAL_PAGES} 页上限（{MAX_CHUNKS} 片 × "
            f"{LIMIT_PAGES} 页）。请拆分后重试，或把 DSH_OXR_BASE_URL 指向自部署 / 正式版 OXR。",
        )
    chunks: list[bytes] = []
    for start in range(0, total, pages_per_chunk):
        writer = PdfWriter()
        for i in range(start, min(start + pages_per_chunk, total)):
            writer.add_page(reader.pages[i])
        buf = io.BytesIO()
        writer.write(buf)
        chunks.append(buf.getvalue())
    return chunks


def parse_pdf_bytes(
    name: str, blob: bytes, *, pages: "int | None" = None, _chunked: bool = False
) -> str:
    """PDF：先看页数与大小，超限才切片；切片结果按序拼接。

    `pages` 是调用方**已经数过**的页数（doc_route 的侦察就数过一次）。
    给了它就不再自己开一遍 pypdf —— 大文件的 `PdfReader(...)` 解析并不便宜，
    而侦察已经把答案拿到了。

    `_chunked` 标的是「这已经是切片后的单片」。服务端仍报超限（1118/1132）时，
    已分片的一律兜底、未分片的才去切片（方案 §7.2 的那两行）—— 不这么做，
    一个被误判为「可以整传」的大文件会白白失败一次，而用户看到的是「云端不支持」。
    """
    total = pages if pages is not None else page_count(blob)
    too_big = len(blob) > LIMIT_BYTES
    too_many = total is not None and total > LIMIT_PAGES
    if not too_big and not too_many:
        try:
            return parse_bytes(name, blob, "application/pdf")
        except OxrError as exc:
            # 我们这边没判出超限、服务端却报了超限：未分片 → 切片重来（方案 §7.2）
            if exc.kind != "too-large" or _chunked:
                raise
            diag("oxr.chunk.retry", f"{name} 被服务端判为超限，改为分片重试")

    chunks = split_pdf(blob)
    if len(chunks) == 1 and len(chunks[0]) > LIMIT_BYTES:
        # 按页切不动（页数没超，是单页就超 10MB 的巨型扫描图）：如实说清，
        # 别让用户以为「切了片还是失败」是服务端的问题。
        raise OxrError(
            "too-large",
            f"这份 PDF 只有 {total if total is not None else '少量'} 页，但体积 "
            f"{len(blob) / 1024 / 1024:.1f} MB 超过云端 "
            f"{LIMIT_BYTES // 1024 // 1024} MB 上限，按页切片也降不下来。"
            "请先压缩页面图片（降低扫描分辨率）后重试，或把 DSH_OXR_BASE_URL "
            "指向自部署 / 正式版 OXR。",
        )
    stem = os.path.splitext(os.path.basename(name))[0] or "document"
    parts: list[str] = []
    for idx, chunk in enumerate(chunks, start=1):
        try:
            part = parse_bytes(f"{stem}.part{idx}.pdf", chunk, "application/pdf")
        except OxrError as exc:
            if exc.kind == "too-large":
                # 已分片还超限：到此为止，说明单页本身就超（方案 §7.2「已分片 → 兜底并说明上限」）
                raise OxrError(
                    "too-large",
                    f"切片后单片仍超过云端上限（第 {idx} 片）。请降低扫描分辨率，"
                    "或把 DSH_OXR_BASE_URL 指向自部署 / 正式版 OXR。",
                ) from exc
            raise
        if part.strip():
            parts.append(part.strip())
    return "\n\n".join(parts)


def parse_file(path, *, pages: "int | None" = None) -> str:
    """解析一个本机文件，返回 Markdown。

    `pages` 是 `doc_route.probe_pdf` 已经数过的页数（能省一次 pypdf 解析）；
    只有 PDF 会用到它，其它格式忽略。
    """
    path = os.fspath(path)
    try:
        with open(path, "rb") as fh:
            blob = fh.read()
    except OSError as exc:
        raise OxrError("unsupported", f"读不到文件：{exc}") from exc
    suffix = os.path.splitext(path)[1].lower()
    name = os.path.basename(path)
    if suffix == ".pdf":
        return parse_pdf_bytes(name, blob, pages=pages)
    return parse_bytes(name, blob, _MIME.get(suffix, "application/octet-stream"))


# ---------- 自测（无网络） ----------


def self_test() -> list[str]:
    """纯函数自测，返回失败清单（空 = 全绿）。供 doc_route.py --self-test 调用。"""
    failures: list[str] = []

    def check(name: str, got, want) -> None:
        if got != want:
            failures.append(f"oxr.{name}: 期望 {want!r}，实际 {got!r}")

    def check_true(name: str, cond) -> None:
        if not cond:
            failures.append(f"oxr.{name}: 期望为真，实际为假")

    # 方案 §10：1123 / 1108 / 1107 都不进重试分支，1101 / 1124 进
    for code, kind in (
        (0, "ok"),
        (1101, "retry"),
        (1124, "retry"),
        (1102, "timeout"),
        (1104, "unsupported"),
        (1106, "unsupported"),
        (1137, "unsupported"),
        (1118, "too-large"),
        (1132, "too-large"),
        (1123, "quota"),
        (1108, "no-order"),
        (1107, "auth"),
        (9999, "unknown"),
    ):
        check(f"classify({code})", classify_code(code), kind)

    for code in (1123, 1108, 1107):
        check(f"should_retry({code})", should_retry(code), False)
    for code in (1101, 1124):
        check(f"should_retry({code})", should_retry(code), True)
    check("should_retry(0)", should_retry(0), False)
    check("should_retry(1102)", should_retry(1102), False)

    # 文案里必须能分清「限额」与「欠费/额度」
    check_true("quota 文案", "额度已用尽" in describe_code(1123, "no pages left"))
    check_true("no-order 文案", "订单" in describe_code(1108))
    check_true("auth 文案", "API Key" in describe_code(1107))

    # 超限的本地预检（不发请求）
    try:
        parse_bytes("big.pdf", b"x" * (LIMIT_BYTES + 1), "application/pdf")
    except OxrError as exc:
        check("超大预检 kind", exc.kind, "too-large")
    else:
        failures.append("oxr.超大预检: 应当抛 too-large")

    # 上限常量的自洽性：分片数 × 单片页数必须等于对外宣称的总上限
    check("MAX_TOTAL_PAGES", MAX_TOTAL_PAGES, LIMIT_PAGES * MAX_CHUNKS)
    check("MAX_CHUNKS", MAX_CHUNKS, 3)

    # 「已分片仍超限」的分支：把 parse_bytes 换成必然抛 too-large 的桩，
    # 断言切片路径**不会**无限重试，而是抛出一句说明上限的话（方案 §7.2）。
    # split_pdf 用桩替代（本机可能没装 pypdf，而这里要验的是编排逻辑不是 pypdf）。
    original_parse = globals()["parse_bytes"]
    original_split = globals()["split_pdf"]
    calls = {"n": 0}

    def _always_too_large(name, blob, mime=""):
        calls["n"] += 1
        raise OxrError("too-large", "stub: 服务端报超页数", code=1132)

    globals()["parse_bytes"] = _always_too_large
    globals()["split_pdf"] = lambda blob, pages_per_chunk=LIMIT_PAGES: [b"chunk1", b"chunk2"]
    try:
        # pages 声明 > 20 → 必然走切片路径；两片各失败一次，但**第一片就停**
        parse_pdf_bytes("stub.pdf", b"%PDF-1.4 stub", pages=25)
    except OxrError as exc:
        check("已分片仍超限 kind", exc.kind, "too-large")
        check_true("已分片仍超限 文案含上限", "上限" in exc.message)
        check("已分片仍超限 只试了一片", calls["n"], 1)
    else:
        failures.append("oxr.已分片仍超限: 应当抛 OxrError")
    finally:
        globals()["parse_bytes"] = original_parse
        globals()["split_pdf"] = original_split

    # 「我们没判出超限、服务端却报超限」→ 未分片要改成分片重试（方案 §7.2）
    calls2 = {"n": 0}

    def _first_too_large_then_ok(name, blob, mime=""):
        calls2["n"] += 1
        if calls2["n"] == 1:
            raise OxrError("too-large", "stub: 服务端报超页数", code=1132)
        return "# 切片结果"

    globals()["parse_bytes"] = _first_too_large_then_ok
    globals()["split_pdf"] = lambda blob, pages_per_chunk=LIMIT_PAGES: [b"chunk1"]
    try:
        got = parse_pdf_bytes("retry.pdf", b"%PDF-1.4 small", pages=3)
        check("服务端报超限→分片重试 结果", got, "# 切片结果")
        check("服务端报超限→分片重试 次数", calls2["n"], 2)
    except Exception as exc:  # noqa: BLE001
        failures.append(f"oxr.服务端报超限→分片重试: 不该失败（{exc}）")
    finally:
        globals()["parse_bytes"] = original_parse
        globals()["split_pdf"] = original_split

    # 缺 pypdf 时的退化文案（真实路径，不装桩）
    if page_count(b"%PDF-1.4 stub") is None:
        try:
            split_pdf(b"%PDF-1.4 stub")
        except OxrError as exc:
            check("缺 pypdf kind", exc.kind, "too-large")
            check_true("缺 pypdf 文案可操作", "基本安装" in exc.message)

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
        print("OK   oxr_client 纯函数自测全部通过")
        raise SystemExit(0)
    print("用法：python oxr_client.py --self-test")
    raise SystemExit(2)
