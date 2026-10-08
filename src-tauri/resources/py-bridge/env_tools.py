# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""环境探测 + 能力发现。

两件事：
  ① `get_python_environment_info` —— 回答「这台机器的 Python 里到底有什么」；
  ② `_python_capabilities_prompt` —— 用 ① 的数据拼一段**系统提示词**注入：
     用户日后自己装了 pandas，这段提示词会自动多出 pandas / df.head() 的说明，
     不需要我们改代码重新发版。

刻意**只回关注清单**：`importlib.metadata.distributions()` 能列出几百个包，
全丢给模型等于用一次性 token 换一屏噪音。
"""

from __future__ import annotations

import sys

import dsh_bridge

# 关注清单：分「基本安装必带」与「可选数据分析」，后者装没装都报
FOCUS_PACKAGES: tuple[str, ...] = (
    "markitdown",
    "dsh-python-bridge",
    "python-docx",
    "python-pptx",
    "openpyxl",
    "XlsxWriter",
    "lxml",
    "Pillow",
    "numpy",
    "pandas",
    "scipy",
    "matplotlib",
    "scikit-learn",
)

# 展示名 → (导入名, 一句话说明)；提示词与 info 工具共用，保证两处不漂移
FOCUS_INFO: tuple[tuple[str, str, str], ...] = (
    ("markitdown", "markitdown", "任意文档 → Markdown"),
    ("python-docx", "docx", "Word 读写"),
    ("python-pptx", "pptx", "PowerPoint 读写"),
    ("openpyxl", "openpyxl", "Excel 读写"),
    ("XlsxWriter", "xlsxwriter", "Excel 写入（样式/图表）"),
    ("lxml", "lxml", "XML 解析"),
    ("Pillow", "PIL", "图片尺寸/处理"),
    ("numpy", "numpy", "数值计算"),
    ("pandas", "pandas", "表格数据处理"),
    ("scipy", "scipy", "科学计算"),
    ("matplotlib", "matplotlib", "绘图"),
    ("scikit-learn", "sklearn", "机器学习"),
)


def _versions() -> dict[str, str]:
    """取关注清单里各包的版本。

    用 `importlib.metadata` 而不是已废弃的 `pkg_resources`（后者在 setuptools
    ≥ 81 已告警、且在未来版本会移除）；取不到就跳过而不是抛错。
    """
    out: dict[str, str] = {}
    try:
        from importlib.metadata import PackageNotFoundError, version
    except ImportError:  # pragma: no cover — Python < 3.8 才有；桥接要求 ≥ 3.10
        return out
    for dist in FOCUS_PACKAGES:
        try:
            out[dist] = version(dist)
        except PackageNotFoundError:
            continue
        except Exception:  # noqa: BLE001 — 元数据损坏不该影响其它包的报告
            continue
    return out


def _environment_payload() -> dict:
    versions = _versions()
    available = []
    missing = []
    for dist, module, desc in FOCUS_INFO:
        row = {"package": dist, "import": module, "description": desc}
        if dist in versions:
            row["version"] = versions[dist]
            # find_spec 说的是「能不能 import」，比「dist-info 在不在」更接近实际可用性
            try:
                import importlib.util as _u

                row["importable"] = _u.find_spec(module) is not None
            except Exception:  # noqa: BLE001
                row["importable"] = False
            available.append(row)
        else:
            missing.append(row)
    return {
        "python_version": sys.version.split()[0],
        "executable": sys.executable,
        "platform": sys.platform,
        "available": available,
        "missing": missing,
    }


@dsh_bridge.tool(
    name="get_python_environment_info",
    description=(
        "报告本机 Python 环境：版本、可执行文件路径，以及 DSH 关注的那些库是否可用"
        "（只报关注清单，不列全部已装包）。"
        "当某个工具报「库不可用 / 某个格式转不了」时先调它，拿到的是确切清单。"
    ),
    parameters={"type": "object", "properties": {}},
)
def get_python_environment_info() -> dict:
    try:
        return {"ok": True, "environment": _environment_payload()}
    except Exception as exc:  # noqa: BLE001
        return {"ok": False, "error": f"{type(exc).__name__}: {exc}"}


@dsh_bridge.system_prompt_section(order=200, text="（运行时填充，见 dsh_bridge.runtime 的 manifest.promptSections）")
def _capabilities_section_placeholder():
    """占位：真正的能力发现文案由宿主在握手后用 init 后的环境数据填充。

    这里保留一个静态提示段作为**兜底**：即便宿主没做二次填充（自定义宿主、
    或未来换掉注册方式），模型也至少知道「有这些工具、结果给 result、长文会截断」。
    措辞与 utils.py 的截断规则、sandbox_tools 的白名单保持一致。
    """
    return (
        "本会话已接入 Python 能力桥接（单进程，dsh-python-bridge）。"
        "可用工具：convert_file_to_markdown / convert_files_to_markdown / markitdown_capabilities、"
        "read_excel_data / create_styled_excel、read_docx_text / generate_word_report、"
        "read_pptx_outline / add_resized_image_to_pptx、get_python_environment_info、"
        "execute_python_sandbox。"
        "约定：execute_python_sandbox 里把结果赋给 `result`；可用库见工具描述，"
        "os / sys / subprocess / 网络 / 文件读写一律被拦截。"
        "所有工具的正文字段超过长度上限时只返回摘要与文件路径，需要全文请自行读该文件。"
    )


def capability_prompt_text() -> str:
    """按当前环境生成能力发现提示词。**这段文案的唯一所有者是 Python 侧。**

    宿主（index.js）不自己拼这段文字，而是调本函数拿 —— 两边各写一份必然漂移：
    白名单变了、措辞改了，提示词却还在说旧话，模型就会照着错的信息行事。

    为什么放在 Python 而不是 JS：`importlib.metadata` 只有 Python 拿得到，
    而「哪些库装了」正是这段文案的核心内容。

    拼出来的东西只有几百字，却能显著减少「模型不知道 pandas 能用」这类误判，
    而且**随用户安装的库自动变化** —— 这正是能力发现机制在这里的价值。
    """
    payload = _environment_payload()
    have = "、".join(
        f"{r['import']}（{r['description']}{'，已装' if r.get('importable') else '，已装但不可导入'}）"
        for r in payload["available"]
    )
    absent = "、".join(r["import"] for r in payload["missing"]) or "无"
    return (
        f"【Python 能力桥接】解释器：Python {payload['python_version']}（{payload['platform']}）。\n"
        f"当前可用库：{have}。\n"
        f"未安装（如需请让用户在首选项「Python 环境」里安装）：{absent}。\n"
        "文档 → Markdown 请用 convert_file_to_markdown（只接受本机路径，不接受 URL）。\n"
        "需要这些工具覆盖不到的逻辑时用 execute_python_sandbox：把结果赋给 `result`，"
        "沙箱禁止 os / sys / subprocess / 网络 / 文件读写，"
        "但放行常见标准库与已安装的数据分析库。\n"
        "拿不准当前环境时，先调 get_python_environment_info 查一次，不要凭猜测使用库名。\n"
        "所有工具返回超长正文时只给摘要 + 文件路径；需要全文请用文件读取工具打开该文件。"
    )


@dsh_bridge.tool(
    name="describe_python_capabilities",
    description=(
        "返回一段**按当前环境实时生成**的能力说明文本（已装哪些库、工具清单、"
        "沙箱约定、输出截断规则），可直接作为系统提示词使用。"
        "宿主在每次组装提示词时调用它，所以用户日后装了新库，这段说明会自动更新，"
        "不需要重启 DSH。人类用户一般不需要直接调它 —— 由 DSH 自动使用。"
    ),
    parameters={"type": "object", "properties": {}},
)
def describe_python_capabilities() -> dict:
    """宿主取能力发现文案的入口。

    刻意做成一个**工具**而不是塞进 initialize 的 manifest：manifest 在握手时定下，
    是快照；工具调用可以发生在任何时刻，才能真正做到「随环境变化」。
    """
    try:
        return {"ok": True, "text": capability_prompt_text()}
    except Exception as exc:  # noqa: BLE001
        return {"ok": False, "error": f"{type(exc).__name__}: {exc}"}