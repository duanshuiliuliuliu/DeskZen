// DeskZen 设置界面冒烟测试（需以 CDP 调试端口启动应用）
//
// 用法：先带 CDP 端口启动应用，再运行
//   node scripts/settings_sidebar_e2e.mjs
import { cdp, getTargets, evaluate, personaTargetFilter } from "./cdp_e2e_lib.mjs";

const personaTarget = await getTargets(personaTargetFilter);
const persona = cdp(personaTarget.webSocketDebuggerUrl);
await persona.ready;
await persona.send("Runtime.enable");

await evaluate(persona, `window.__TAURI_INTERNALS__.invoke("open_settings")`);

let settingsTarget = null;
for (let i = 0; i < 20; i++) {
  await new Promise((r) => setTimeout(r, 500));
  settingsTarget = await getTargets((t) => t.type === "page" && t.url.includes("settings"));
  if (settingsTarget) break;
}
if (!settingsTarget) throw new Error("设置窗口未打开");

const settings = cdp(settingsTarget.webSocketDebuggerUrl);
await settings.ready;
await settings.send("Runtime.enable");

// 1. 侧边栏结构：三个入口；先点回「角色」，避免复用已打开的设置窗时停在别的面板
const init = await evaluate(
  settings,
  `(async () => {
     [...document.querySelectorAll(".nav-item")].find((b) => b.dataset.panel === "role").click();
     await new Promise((r) => setTimeout(r, 500));
     return {
       nav: [...document.querySelectorAll(".nav-item")].map((b) => b.textContent.trim()),
       active: document.querySelector(".nav-item.active")?.dataset.panel,
       hasSidebar: !!document.querySelector(".settings-sidebar"),
       panels: ["panel-role", "panel-llm", "panel-log", "panel-about"].filter((id) => document.getElementById(id)).length,
       version: document.getElementById("app-version").textContent.trim(),
     };
   })()`,
);
console.log("初始:", JSON.stringify(init));
if (
  init.nav.join(",") !== "角色,大模型,日志,关于" ||
  init.active !== "role" ||
  !init.hasSidebar ||
  init.panels !== 4
) {
  throw new Error("侧边栏初始状态异常");
}
// 版本号来自运行时 tauri.conf.json；取不到时会保留占位符
if (!/^\d+\.\d+\.\d+/.test(init.version)) {
  throw new Error(`关于面板版本号异常: ${init.version}`);
}

// 2. 点击侧边栏可切换激活项
const llmState = await evaluate(
  settings,
  `(async () => {
     [...document.querySelectorAll(".nav-item")].find((b) => b.dataset.panel === "llm").click();
     await new Promise((r) => setTimeout(r, 300));
     return document.querySelector(".nav-item.active")?.dataset.panel;
   })()`,
);
if (llmState !== "llm") throw new Error(`切换「大模型」失败: ${llmState}`);

// 3. 关键元素均存在
const ids = [
  "passthrough",
  "persona-zoom",
  "local-import",
  "local-import-zip",
  "local-import-drop",
  "imported-list",
  "base-url",
  "model",
  "api-key",
  "temperature",
  "max-tokens",
  "ai-bubbles",
  "llm-save",
  "quit-app",
];
const missing = await evaluate(
  settings,
  `(${JSON.stringify(ids)}).filter((id) => !document.getElementById(id))`,
  false,
);
console.log("缺失元素:", JSON.stringify(missing));
if (missing.length > 0) throw new Error(`设置界面缺少元素: ${missing.join(",")}`);

// 4. 切回「角色」，角色列表应已渲染（内置林克至少一行）
const roleState = await evaluate(
  settings,
  `(async () => {
     [...document.querySelectorAll(".nav-item")].find((b) => b.dataset.panel === "role").click();
     await new Promise((r) => setTimeout(r, 300));
     return {
       active: document.querySelector(".nav-item.active")?.dataset.panel,
       importedCount: document.querySelectorAll("#imported-list .imported-item").length,
     };
   })()`,
);
console.log("切回角色:", JSON.stringify(roleState));
if (roleState.active !== "role" || roleState.importedCount === 0) {
  throw new Error("切回「角色」或角色列表异常");
}

persona.close();
settings.close();
console.log("设置界面端到端验证通过 ✓");
