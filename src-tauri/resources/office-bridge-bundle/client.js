// DSH Desktop —— OnlyOffice 能力桥接：Client 半身（P2，诊断版）
//
// ⚠ 本版是「重度诊断」：激活期每一步的 typeof / try-catch 全部打到 console，
//   用于定位 v1 的 "1 entry did not activate — failed"。定位后会把诊断收敛成 2-3 行。
//
// 装载合同（实测确认）：boot graph（由 package.json 的 dsh.client 声明组合）→
// 浏览器按 combo URL 取脚本 → 必须是 **window.__ModuleLoader__.load({id, factory})**
// 的模块表注册脚本（顶层 return 会 SyntaxError: Illegal return statement——v2 踩过）；
// factory materialize 出 exports → cordis 把 exports 当插件激活（exports.inject 是服务闸门）。
// React 从模块表 require（平台单例，实测可解析）。

window.__ModuleLoader__.load({
	id: "@local/dsh-desktop-office-bridge",
	factory: (require) => {
		const log = (...a) => { try { console.log("[office-bridge][client]", ...a) } catch {} }
		const logErr = (...a) => { try { console.error("[office-bridge][client]", ...a) } catch {} }
		log("factory 开始");
		var module = { exports: {} };
		var exports = module.exports;
		Object.defineProperty(exports, Symbol.toStringTag, { value: "Module" });
		const React = require("react");
		const h = React.createElement;
		log("factory：react 已解析", typeof React.createElement);

		const PKG_ID = "@local/dsh-desktop-office-bridge";
		const KIND = "office-docs";
		const OFFICE_EXTS = ["docx", "doc", "odt", "rtf", "txt", "epub", "xlsx", "xls", "ods", "csv", "pptx", "ppt", "odp"];
		const S = {
			root: { height: "100%", display: "flex", flexDirection: "column", fontFamily: "inherit" },
			fill: { flex: 1, position: "relative", minHeight: 0 },
			iframe: { position: "absolute", inset: 0, width: "100%", height: "100%", border: "0" },
			bar: { display: "flex", alignItems: "center", gap: 8, padding: "6px 10px", borderBottom: "1px solid rgba(127,127,127,.25)", fontSize: 13 },
			btn: { padding: "4px 10px", borderRadius: 6, border: "1px solid rgba(127,127,127,.4)", background: "transparent", color: "inherit", cursor: "pointer", fontSize: 13 },
			btnPrimary: { padding: "5px 12px", borderRadius: 6, border: "1px solid transparent", background: "#2f6feb", color: "#fff", cursor: "pointer", fontSize: 13 },
			row: { display: "flex", alignItems: "center", gap: 8, padding: "7px 4px", borderBottom: "1px solid rgba(127,127,127,.15)", fontSize: 13 },
			name: { flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" },
			muted: { opacity: 0.65, fontSize: 12 },
			err: { color: "#d33", fontSize: 12, margin: "6px 0" },
			card: { margin: "24px auto", maxWidth: 420, textAlign: "center", fontSize: 13, lineHeight: 1.7 },
		};

		// ---- 文档服务发现：Client 半身拿不到 Host 的 config（§12.2），从浏览器侧探端口 ----
		let baseCache = { base: undefined, at: 0 }
		function discoverBase() {
			if (typeof baseCache.base === "string" && Date.now() - baseCache.at < 5 * 60_000) {
				return Promise.resolve(baseCache.base)
			}
			if (baseCache.base === null && Date.now() - baseCache.at < 15_000) {
				return Promise.resolve(null)
			}
			baseCache.at = Date.now()
			const tryPort = (port) => fetch(`http://127.0.0.1:${port}/status`, { signal: AbortSignal.timeout(1200) })
				.then((r) => (r.ok ? r.json() : null))
				.then((j) => (j && typeof j.docserverHealthy === "boolean" ? `http://127.0.0.1:${port}` : null))
				.catch(() => null)
			return tryPort(3310)
				.then((hit) => {
					if (hit) return hit
					const rest = []
					for (let port = 3311; port <= 3319; port++) rest.push(tryPort(port))
					return Promise.all(rest).then((rs) => rs.find(Boolean) || null)
				})
				.then((found) => { baseCache.base = found || null; return baseCache.base })
		}

		// ---- 在系统默认浏览器里打开 ----
		// 内嵌 webview（DSH Desktop 的 Tauri 子 webview）里 window.open 开不出新窗口，点了静默无事发生；
		// 改由 Host 半身用 rundll32 拉起系统默认浏览器。Host 不在时才退回 window.open（真浏览器里够用）。
		function openInSystemBrowser(base, docPath, fallbackUrl, onError) {
			const openWindow = () => { try { window.open(fallbackUrl, "_blank", "noopener,noreferrer") } catch (e) {} }
			if (!base || !docPath) { openWindow(); return }
			fetch(base + "/browse?path=" + encodeURIComponent(docPath), { method: "POST" })
				.then((r) => r.json())
				.then((j) => { if (!j || !j.ok) throw new Error((j && j.error) || "打开失败") })
				.catch((e) => { openWindow(); if (onError) onError("没能拉起浏览器：" + String((e && e.message) || e)) })
		}

		function extOf(p) {
			const i = String(p || "").lastIndexOf(".")
			return i >= 0 ? p.slice(i + 1).toLowerCase() : ""
		}

		function OfficeDocsBody(props) {
			const useTabInfo = props && props.useTabInfo
			const info = typeof useTabInfo === "function" ? useTabInfo() : null
			const nav = info && info.tab ? info.tab.navigation : null
			const params = nav ? nav.params : null
			const revision = nav ? nav.revision : 0

			const [base, setBase] = React.useState(undefined)
			const [docs, setDocs] = React.useState(null)
			const [editor, setEditor] = React.useState(null)
			const [error, setError] = React.useState(null)
			const [editorMsg, setEditorMsg] = React.useState(null)

			React.useEffect(() => {
				let dead = false
				discoverBase().then((b) => { if (!dead) setBase(b) })
				return () => { dead = true }
			}, [])

			const refreshDocs = React.useCallback(() => {
				if (!base) return
				let dead = false
				fetch(base + "/docs").then((r) => r.json())
					.then((j) => { if (!dead) { setDocs(j.docs || []); setError(null) } })
					.catch((e) => { if (!dead) setError(String((e && e.message) || e)) })
				return () => { dead = true }
			}, [base])
			React.useEffect(() => refreshDocs(), [refreshDocs])

			const navPath = params && params.path
			React.useEffect(() => {
				if (!base || !navPath) return
				let dead = false
				fetch(base + "/open?path=" + encodeURIComponent(navPath))
					.then((r) => r.json())
					.then((j) => {
						if (dead) return
						if (j && j.editorUrl) { setEditor({ url: j.editorUrl, name: j.name, path: j.path }); setError(null) }
						else setError((j && j.error) || "打开失败")
					})
					.catch((e) => { if (!dead) setError(String((e && e.message) || e)) })
				return () => { dead = true }
			}, [base, navPath, revision])

			React.useEffect(() => {
				const onMsg = (ev) => {
					const d = ev && ev.data
					if (d && d.source === "dsh-office" && d.type === "error") {
						setEditorMsg("编辑器报错：" + JSON.stringify(d.detail).slice(0, 160))
					}
				}
				window.addEventListener("message", onMsg)
				return () => window.removeEventListener("message", onMsg)
			}, [])

			if (base === undefined) return h("div", { style: S.card }, "正在连接 OnlyOffice 文档服务…")
			if (base === null) {
				return h("div", { style: S.card },
					h("div", null, "OnlyOffice 文档服务未运行。"),
					h("div", { style: S.muted }, "请在 DSH Desktop 的「安装/卸载」里确认 OnlyOffice 已安装且服务在运行。"),
					h("button", { style: Object.assign({}, S.btn, { marginTop: 10 }), onClick: () => { baseCache.at = 0; setBase(undefined); discoverBase().then(setBase) } }, "重试"))
			}

			if (editor) {
				return h("div", { style: S.root },
					h("div", { style: S.bar },
						h("button", { style: S.btn, onClick: () => setEditor(null) }, "← 文档列表"),
						h("span", { style: Object.assign({}, S.name, { fontWeight: 600 }) }, editor.name || ""),
						h("button", { style: S.btn, onClick: () => openInSystemBrowser(base, editor.path, editor.url, setEditorMsg) }, "在浏览器打开")),
					editorMsg ? h("div", { style: S.err }, editorMsg) : null,
					h("div", { style: S.fill },
						h("iframe", { src: editor.url, style: S.iframe, title: editor.name || "OnlyOffice 编辑器" })))
			}

			const onUpload = (ev) => {
				const file = ev.target.files && ev.target.files[0]
				ev.target.value = ""
				if (!file) return
				setError(null)
				file.arrayBuffer().then((buf) =>
					fetch(base + "/upload?name=" + encodeURIComponent(file.name), { method: "POST", body: buf })
						.then((r) => r.json())
						.then((j) => {
							if (j && j.editorUrl) { setEditor({ url: j.editorUrl, name: j.name, path: j.path }); refreshDocs() }
							else setError((j && j.error) || "上传失败")
						})
						.catch((e) => setError(String((e && e.message) || e))))
			}
			const openDoc = (d) => {
				setError(null)
				fetch(base + "/open?path=" + encodeURIComponent(d.path))
					.then((r) => r.json())
					.then((j) => { if (j && j.editorUrl) setEditor({ url: j.editorUrl, name: j.name, path: j.path }); else setError((j && j.error) || "打开失败") })
					.catch((e) => setError(String((e && e.message) || e)))
			}

			return h("div", { style: S.root },
				h("div", { style: S.bar },
					h("label", { style: S.btnPrimary }, "打开本地文档…",
						h("input", { type: "file", accept: "." + OFFICE_EXTS.join(",."), style: { display: "none" }, onChange: onUpload })),
					h("button", { style: S.btn, onClick: refreshDocs }, "刷新")),
				error ? h("div", { style: Object.assign({}, S.err, { padding: "0 12px" }) }, error) : null,
				h("div", { style: S.body },
					docs === null ? h("div", { style: S.muted }, "加载中…")
						: docs.length === 0 ? h("div", { style: S.muted }, "文档目录里还没有文件。点上面「打开本地文档」上传一份，或让 AI 用 office_open 打开工作区里的文档。")
							: docs.map((d) => h("div", { key: d.path, style: S.row },
								h("span", { style: S.name, title: d.path }, d.name),
								h("span", { style: S.muted }, (d.size / 1024).toFixed(1) + " KB"),
								h("button", { style: S.btn, onClick: () => openDoc(d) }, "编辑"),
								h("button", { style: S.btn, onClick: () => openInSystemBrowser(base, d.path, d.editorUrl, setError) }, "浏览器")))))
		}

		function EditWithOfficeAction(props) {
			const absolutePath = props && props.absolutePath
			if (!absolutePath || !OFFICE_EXTS.includes(extOf(absolutePath))) return null
			const [base, setBase] = React.useState(undefined)
			const [busy, setBusy] = React.useState(false)
			React.useEffect(() => {
				let dead = false
				discoverBase().then((b) => { if (!dead) setBase(b) })
				return () => { dead = true }
			}, [])
			if (base === undefined || base === null) return null
			const onClick = () => {
				if (busy) return
				setBusy(true)
				fetch(base + "/open?path=" + encodeURIComponent(absolutePath))
					.then((r) => r.json())
					.then((j) => {
						if (j && j.editorUrl && j.path && openTabRef) openTabRef(KIND, { params: { path: j.path } })
					})
					.catch(() => {})
					.finally(() => setBusy(false))
			}
			return h("button", {
				style: S.btn,
				onClick,
				disabled: busy,
				title: "在右侧栏「文档」页中用 OnlyOffice 编辑此文件",
			}, busy ? "打开中…" : "用 OnlyOffice 编辑")
		}

		// openTab 由 apply 里捕获的 ctx.sidebarRight 提供（组件经此引用调用）
		let openTabRef = null

		function apply(ctx) {
			// ---- 诊断：激活环境的全貌 ----
			log("apply 开始");
			log("  Object.keys(ctx):", JSON.stringify(Object.keys(ctx)));
			log("  typeof ctx.effect:", typeof ctx.effect, "| ctx.get:", typeof ctx.get);
			log("  typeof ctx.slots:", typeof ctx.slots, "| ctx.sidebarRightTabs:", typeof ctx.sidebarRightTabs, "| ctx.sidebarRight:", typeof ctx.sidebarRight);
			try { log("  ctx.get('slots'):", typeof ctx.get("slots")) } catch (e) { log("  ctx.get('slots') 抛错:", e.message) }
			try { log("  ctx.get('sidebarRightTabs'):", typeof ctx.get("sidebarRightTabs")) } catch (e) { log("  ctx.get('sidebarRightTabs') 抛错:", e.message) }

			// ① tab 类型（含「开始」页胶囊）
			try {
				const d = ctx.sidebarRightTabs.register({
					id: PKG_ID,
					kind: KIND,
					title: () => "文档（OnlyOffice）",
					keepMounted: true,
					guide: [{
						id: "office-docs",
						order: 50,
						title: () => "文档（OnlyOffice）",
						description: () => "打开、上传并编辑 docx / xlsx / pptx",
					}],
				});
				ctx.effect(() => d, "office-bridge: tab type");
				log("① tab 类型注册 OK");
			} catch (e) { logErr("① tab 类型注册抛错：", (e && e.stack) || String(e)); }

			// ② 面板体
			try {
				const d = ctx.slots.inject("sidebar.right.pane.tab", () => ctx.slots.register({
					name: "sidebar.right.pane.tab",
					key: PKG_ID,
				}, OfficeDocsBody));
				ctx.effect(() => d, "office-bridge: tab body");
				log("② 面板体注册 OK");
			} catch (e) { logErr("② 面板体注册抛错：", (e && e.stack) || String(e)); }

			// ③ 预览工具栏按钮
			try {
				const d = ctx.slots.inject("sidebar.right.tab.document.actions", () => ctx.slots.register({
					name: "sidebar.right.tab.document.actions",
					id: "office-bridge-edit",
				}, EditWithOfficeAction));
				ctx.effect(() => d, "office-bridge: preview action");
				log("③ 预览按钮注册 OK");
			} catch (e) { logErr("③ 预览按钮注册抛错：", (e && e.stack) || String(e)); }

			// openTab 接线
			try { openTabRef = (kind, opts) => ctx.sidebarRight.openTab(kind, opts); log("④ openTab 已接线") } catch (e) { logErr("④ openTab 接线抛错：", e.message) }

			log("apply 结束");
		}

		exports.apply = apply;
		exports.inject = ["slots", "sidebarRightTabs", "sidebarRight"];
		return module.exports;
	}
});
