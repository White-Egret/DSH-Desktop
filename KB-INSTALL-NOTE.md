# 「记忆，技能与知识库」安装块 —— 交付说明

本次给「安装/卸载」弹窗（`manage-modal`）加了一个块，一颗按钮、一行合并状态，一次装齐两个 DSH 插件：

- **dsh-kb 0.4.2**（知识层：`kb_scan/extract/write/status/search` + 词典 `dict_lookup/dict_search` + 页面图谱；0.4.2 = inject 修复 + vault 默认 `D:/KB-Vault`）
- **dsh-memory2skill 0.4.9**（记忆/技能层：逐字记录纠正 → 雷达 → 起草 SKILL.md → 门禁 → 人工 commit；0.4.9 = inject 修复发布版，另含 0.4.7/0.4.8 以来新增的 `memory2skill_recall`）

设计依据是 `D:\DSH\Workspace\plugins\dsh-kb-plan.md` 第八章。**后端模板是 `office.rs`**（`py_bridge` 刻意没有安装命令），前端三件套照 office 那套，事件走 `target="kb"`。

## 改了什么

| 文件 | 改动 |
|---|---|
| `src-tauri/resources/kb-bundle/`（新） | 两个成品 tgz，从 plugins 目录**拷入**（源目录未改动） |
| `src-tauri/src/kb.rs`（新，约 940 行） | `KbPhase` + `kb_phase`（纯函数四态）、`kb_merged_message`（纯函数合并文案）、`kb_status` / `kb_install` / `kb_uninstall` 三条命令、`activate_pending` + `cleanup_stale_kb_dirs` 进门自愈、12 条单测 |
| `src-tauri/src/config.rs` | 新增 **Config 正式字段** `kb_pending`（值 = 还差注册的包名逗号列表）+ 默认值 + 2 条往返单测 |
| `src-tauri/src/process.rs` | `set_status("running")` 那条线程里加 `kb::cleanup_stale_kb_dirs` + `kb::activate_pending`，**单独一层 `catch_unwind`**（office 链 panic 也不带走 kb 链）；`save_config` 加 `kb_pending` 的「缺字段沿用磁盘值」兜底 |
| `src-tauri/src/lib.rs` | `mod kb;` + `kb::kb_status / kb_install / kb_uninstall` |
| `src-tauri/src/i18n.rs` | 30 条 `kb_*` 后端文案（zh/en 同键） |
| `src/index.html` | `kb-block` 插在 office-block 之后、uninstall-block 之前 |
| `src/main.js` | `kbTask` / `kbState` + `refreshKbStatus` / `renderKbStatus` / `renderKbButtons` / `setKbProgress` / `setKbSpinner` / `appendKbLog` / `runKbInstall` / `onKbResult`；`onSetupStatus` 按 `kbTask.active`、`onSetupResult` 按 `target==='kb'` 分流；按钮接线；开弹窗与切语言时刷新 |
| `src/i18n.js` | 11 条 `kb_*` 前端键（zh/en 各一份） |
| `scripts/verify-include-str.js` | 扩一条检查：`kb.rs` 的 `include_bytes!` ↔ `resources/kb-bundle/*.tgz` 一一对应（见下面「升级怎么升」） |

## 可行性边界（方案 8.6 那条要求做不到，已按现有模型改）

8.6 原写「`kb_status` 改读宿主 `listBundles`」。**核实后 CLI 化不了**：`dsh plugin` 只是 pnpm 转发器（`add` / `remove` / `why`），没有报告 DSH 解析状态的 `list`；`listBundles` 是**运行中宿主**的运行时服务，Desktop 在 DSH 进程外拿不到。

所以状态判定照 office 的既有模型：`py_bridge::bundle_registered_named(profile, pkg)` 查 `<profile>/node_modules/<pkg>/package.json` 是否存在 + `kb_pending` 标记。用 `file:<tgz>`（不是 `link:`）保证这个判据可信。

**因此「已注册」不等于「已激活」。** 文案的诚实口径（`kb.rs::package_phrase`，单测 `active_with_pending_does_not_claim_activated` 守住）：

- 已注册进 profile 但 pending 仍挂着 → 「已注册进 profile「web」，进入 DSH 后生效」
- 两个都注册好且 pending 已清 → 「记忆与知识库已就绪（profile「web」）：重启 DSH 后 AI 即可调用…」（说的是「重启后」，不说「现在就能用」）
- **没有一处**把「文件系统里有」写成「工具已可用」

可选增强（本轮未做）：DSH 在跑时（端口占用）HTTP 问宿主插件清单，确认 bundle 是否带 `error`，把「已注册」升级成「已激活」。

## 状态机与部分失败

`KbPhase` 四态（两个包各算一份，共用同一个「tgz 已释放」事实）：`NotInstalled` / `NeedsRelease` / `PendingActivate` / `Active`。

合并文案：四种主组合照 8.6（双就绪 / 只有知识库 / 只有记忆层 / 都没装），**其余任何形状一律逐包点名**（`kb_msg_each` 里直接带上 `dsh-kb`、`dsh-memory2skill` 两个包名），所以「只装成一个」不可能被笼统的「已就绪」盖掉。安装结果同理：`kb_register_fail` 带包名 + 原始输出尾部，**不回滚已成功的**，成功的从 pending 去掉、失败的留着。

`kb_pending` 是**列表**而不是布尔，就是为了这个：只重试没装上的那一个。

## pending 的三条防线（照 config.rs 里那两条真机教训）

1. `kb_pending` 是 `Config` 正式字段（否则任何一次 load → 整体写回都会把它当未知键抹掉）；
2. 读写走 `write_config_key` / 单键读-改-写（不整份覆盖别的设置）+ `save_config` 的「缺字段沿用磁盘值」兜底；
3. `activate_pending` 自愈进门：**判据不止 pending** —— 只要「tgz 已释放且 profile 里没这个包」就补注册，标记被抹掉也能恢复。

往返单测 `config::tests::kb_pending_survives_serialize_roundtrip`（含部分失败那个形状 `"dsh-memory2skill"`）+ 老配置缺键回落 `legacy_config_without_kb_pending_falls_back_to_empty`。

## 竞态与卸载的两处纪律

- **DSH 在跑** → 不碰它正读着的 profile，只写完整 pending 后收工，状态行「已写入，进入 DSH 后生效」，进门自愈补注册（照 `deploy_office_after_install`）。
- **卸载** → `dsh plugin remove` 摘两个包（手删 node_modules 会留幽灵清单）→ 清 pending → 删 `kb-bundle` 目录。目录被占用（os error 32/5）删不掉时**补删里面的 tgz**：自愈链的判据是「tgz 已释放 + profile 里没它」，留着 tgz 就等于留着「下次进入 DSH 把刚卸载的东西装回来」的开关。单测 `locked_dir_stops_self_heal_by_emptying_it` 守这条。

## 版本钉死取舍

tgz 是 `include_bytes!` 在构建期嵌进二进制的，文件名带版本号 —— **升级插件要重新构建 Desktop**，与 office 嵌源码同一性质，可接受。

升级步骤：往 `src-tauri/resources/kb-bundle/` 放新 tgz → 改 `kb.rs` 里的 `KB_TGZ` / `MEMORY_TGZ` 两个常量 + 两处 `include_bytes!` 路径 → 重新构建。包名常量不用动。**忘改 `include_bytes!` 时编译不会报错**（新版本躺在目录里但没被嵌进去，装出来的仍是旧版），所以把它接进了 `npm run verify:include-str`：正反两个方向都有单测级验证（临时改名 / 临时多放一个包，脚本都会 FAIL）。另有 Rust 单测 `embedded_bundles_match_package_table` 校验两张常量表一一对应、且嵌进来的字节是 gzip 且非空。

## 这个块刻意不做的事

- **不重实现 DSH 自带的安装语义**（inspect 失败分类、安装源探测、脚本审批、取消/重试）—— 侧边栏「插件」页与 `plugin_manager` 已经做得更完整。本块只做编排：一次装两个包 + tgz 随 Desktop 分发 + 一行说人话的合并状态。状态行的提示文案里明写「也可在 DSH 侧边栏「插件」页单独管理」。
- **没有「重新部署」按钮**：按钮只在未装时是「安装…」、装好后变「卸载…」；注册失败由进门自愈重试。（若只装成一个，按钮已经是「卸载…」，此时想补装失败的那个也靠自愈，不靠再点一次 —— 这正是方案 8.1 那条被删按钮的教训。）
- 没有词典入库触发按钮（低频耗时批处理，归终端/Agent）。
- 没给本块设 Python 前置条件（插件本体零 Python 依赖；`kb_hint_html` 里写明「本体零 Python 依赖，不装「基本安装」也能用」）。
- 两个包没有合并成一个 npm 包。

## 验收清单落地情况

| # | 检查 | 状态 |
|---|---|---|
| 10 | phase 纯函数四态单测 | ✅ `kb_phase_truth_table` |
| 9 | pending 往返单测 | ✅ `kb_pending_survives_serialize_roundtrip` |
| 11 | Active+pending 不谎称已激活 | ✅ 两条单测（短语级 + 合并级） |
| 5 | 只装成一个要点名 | ✅ `partial_install_names_the_missing_layer` |
| 12 | `cargo check` + `cargo test` | ✅ 见下 |
| 1–4、6–8 | 需要真机点按钮 / 看 DSH 工具列表 | ⬜ 未执行（见「没验的部分」） |

## 验证做了什么、没做什么

**做了：**

- `cargo check --all-targets`：通过（只剩 3 条与本次无关的既有 dead_code 警告）。
- `cargo test --lib`：**223 passed / 0 failed**（本机原本报「link.exe not found」——VS 装在非默认路径 `D:\Programs\VisualStudio` + SDK `C:\Program Files (x86)\Windows Kits\10\10.0.28000.0`，按命令给 `PATH/INCLUDE/LIB` 指过去就能链接；没有改任何系统设置）。新增 14 条用例全绿。
- 仓库既有护栏脚本全绿：`check:i18n-args`（它抓出了我 4 处裸参数 —— 这个坑项目里摔过三次）、`verify:include-str`（含我新加的 kb 一条，正反两向实测有效）、`check:spawn-recursion`、`check:versions`。
- 前端：`node --check main.js/i18n.js`；`kb-status` / `btn-kb-install` / `kb-progress` / `kb-progress-text` / `kb-log` 五个 id 在 `index.html` 都有定义；11 个 `kb_*` 键 zh/en 各一份；HTML 引用的键都在词典里；`kb-block` 位置确认在 office-block 之后、uninstall-block 之前。
- 合并文案的组合逻辑与真实 zh 文案做过一遍交叉核对（临时脚本读 `i18n.rs` 取串、跑同样的分支逻辑），核对完即删，不留第二份真源。

**没做（需要你在真机上跑）：**

- 没有实机点过「安装…/卸载…」，所以验收 1–4、6–8 仍是未验证状态：进度滚动、`dsh plugin add file:<tgz>` 在你机器上对这两个包的真实耗时与输出、装完重启 DSH 后 `kb_status` 与 `memory2skill_*` 是否都出现在工具列表、卸载后 pnpm 里两个包是否都干净。
- 没做 Tauri 全量打包（`cargo build` / `npm run tauri build`），只到 `check` + 测试链接为止；安装包体积会增加约 340 KB（两个 tgz 嵌进二进制）。
- `dsh plugin add file:` 这条路我只按 office 的既有实现照抄（office 在你机器上是跑通过的），差别是少一步 `npm pack`。

**已知边界**：状态判据是文件系统 + pending，问不到运行中的宿主，所以「已就绪」始终意味着「重启 DSH 后生效」；要收窄这个缝隙就上文那条 HTTP 查 `listBundles` 的可选增强。

---

## 2026-11-12 第二轮：inject 修复随 Desktop 分发

真机抓出的 inject bug（`inject=['tools']` 却访问 `ctx.systemPrompt` → 激活失败 / persona 静默跳过）已在两个插件源码修好，本轮让修复随 Desktop 分发：

- **dsh-kb 0.4.1 → 0.4.2**（inject 修复 + vault 默认 `D:/KB-Vault` + 去 Obsidian 措辞；Node 134 / Python 52 全绿），从 `plugins/dsh-kb/dist-snapshots/` 拷入。
- **dsh-memory2skill 0.4.6 → 0.4.9**（源码早已是 0.4.8 并新增 `memory2skill_recall`；本轮 bump 0.4.9 标记 inject 修复发布并重建）。
- `src-tauri/src/kb.rs`：`KB_TGZ` / `MEMORY_TGZ` 两个常量 + 两处 `include_bytes!` 路径一起改（`KB_PACKAGES` 走常量自动跟随）。
- resources 换包：删 0.4.1 / 0.4.6，放入 0.4.2（164,404 B）与 0.4.9（187,151 B）。
- 验证：四个护栏脚本全绿；`cargo test --lib` 223 passed（`embedded_bundles_match_package_table` 校验的已是新字节）；`node --check` 前端两文件通过。

**memory2skill 的 `npm run check` 有一处既有漂移**（与本次无关、未修）：`scripts/host-check.mjs:70` 期望 **8** 个工具，但源码在 0.4.8 打包之后新增了 `memory2skill_recall`（现注册 **9** 个），host-check 因此报「应注册 8 个工具，实际 9」。0.4.8 的成品包里确实只有 8 个 —— 漂移在仓库里早于本轮。本轮按约定跳过该子步骤（typecheck / build / test 145 例 / verify-skills 全绿）；下次动插件仓库时把 host-check 的期望改成 9 即可（`scripts/` 不进 tgz，改它不影响成品）。

**推送提醒**：拷贝 DSH-Desktop 到本地仓库后，`resources/kb-bundle/` 的两个**新** tgz（`dsh-kb-0.4.2.tgz`、`dsh-memory2skill-0.4.9.tgz`）必须入库 —— `.gitignore` 的例外已放行，普通 `git add .` 即可；旧 tgz（0.4.1 / 0.4.6）已删，别再把它们加回来。`src-tauri/.cargo/` 仍被忽略（里面是本机绝对路径的 target-dir 配置，别提交）。
