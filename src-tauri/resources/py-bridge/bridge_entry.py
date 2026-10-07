# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）

"""桥接唯一入口：`python -u -m dsh_bridge.runtime bridge_entry`

**单进程多工具**是硬性要求：runtime 只 import 这一个模块，所有工具模块在
import 期间把 `@tool` 注册进同一个全局 registry，之后统一走同一条 stdio 上的
NDJSON JSON-RPC。跨库共享对象（MarkItDown 实例、已打开的 workbook）、
省一份解释器内存、装饰器一次性批量注册 —— 三个好处都是「单进程」才有的。

这里 import 各工具模块**必须放在顶层**：装饰器要在 import 期注册，放进函数里
就会晚于 runtime 建路由表，工具根本不会出现在 manifest 里。

注意：工具模块之间用**平铺** import（`import markitdown_tools`）而不是包相对
import —— runtime 的 module 参数是点分路径，且 cwd 就是本目录，平铺导入最省事，
也不会和宿主环境里恰好存在的同名包打架（前面板同名会让本目录的版本优先）。
"""

from __future__ import annotations

# 顺序不重要（没有依赖关系），但**全部必须 import**：少一个就少一组工具。
import docx_tools  # noqa: F401  （import 即注册）
import env_tools  # noqa: F401
import excel_tools  # noqa: F401
import markitdown_tools  # noqa: F401
import pptx_tools  # noqa: F401
import sandbox_tools  # noqa: F401

# 供宿主侧「自定义模块」模式复用：模块列表导出成常量，避免两处各写一份
TOOL_MODULES = (
    markitdown_tools,
    excel_tools,
    docx_tools,
    pptx_tools,
    sandbox_tools,
    env_tools,
)