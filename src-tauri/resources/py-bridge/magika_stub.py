# DSH Desktop —— Python 能力桥接（生成文件，请勿手工编辑）
# 由 src-tauri/src/py_bridge.rs 的 BRIDGE_FILES 释放；改动请改这里后重新构建桌面端。

"""先 import 本模块，再 import markitdown，可绕过 onnxruntime 的导入期崩溃。

背景：markitdown 顶层**无条件** `import magika`，而 `magika → onnxruntime` 在部分
Python 3.14 / Windows 机器上会在导入期触发原生 access violation（0xC0000005，
进程级崩溃，try/except 拦不住 —— 真机实测 2026-10-11）。桥接自己用的也是同一套
办法（见 markitdown_tools.py 的 `_install_magika_stub`），这里只是把它做成一个
**用户脚本也能直接 import** 的模块。

用法：

    import magika_stub   # noqa: F401  必须先于 markitdown
    import markitdown

这个模块只替换 magika 的**文件类型猜测**：`Magika().identify_stream()` 永远返回
非 "ok" 状态，markitdown 会退回「扩展名 + mimetypes」判断 —— 与桥接运行时完全一致，
docx / pdf / pptx / html 等转换不受影响。

前提：解释器的 `site-packages` 里有一个 `magika-stub.pth` 指向 py-bridge 目录
（桌面端部署时自动写入，见 `ensure_magika_stub_pth`）。没有它，本模块只能被
桥接进程 import 到，普通脚本找不到。
"""

import sys
import types

if "magika" not in sys.modules:
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

        def identify_path(self, *args, **kwargs):
            return _Result()

    module.Magika = Magika
    sys.modules["magika"] = module
