// DeskZen 设置侧边栏冒烟测试（需 CDP 调试端口）
import { cdp, getTargets, evaluate, personaTargetFilter } from "./petdex_e2e_lib.mjs";

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

// 1. 初始状态：侧边栏两项，角色面板可见，大模型面板隐藏
const init = await evaluate(
  settings,
  `(async () => {
     await new Promise((r) => setTimeout(r, 400));
     const nav = [...document.querySelectorAll(".nav-item")].map((b) => b.textContent.trim());
     const active = document.querySelector(".nav-item.active")?.textContent.trim();
     return {
       nav,
       active,
       roleVisible: !document.getElementById("panel-role").classList.contains("hidden"),
       llmHidden: document.getElementById("panel-llm").classList.contains("hidden"),
       hasSidebar: !!document.querySelector(".settings-sidebar"),
     };
   })()`,
);
console.log("初始:", JSON.stringify(init));
if (
  init.nav.join(",") !== "角色设置,大模型设置" ||
  init.active !== "角色设置" ||
  !init.roleVisible ||
  !init.llmHidden ||
  !init.hasSidebar
) {
  throw new Error("侧边栏初始状态异常");
}

// 2. 切换到“大模型设置”
const llmState = await evaluate(
  settings,
  `(async () => {
     [...document.querySelectorAll(".nav-item")]
       .find((b) => b.dataset.panel === "llm")
       .click();
     await new Promise((r) => setTimeout(r, 200));
     return {
       active: document.querySelector(".nav-item.active")?.dataset.panel,
       roleHidden: document.getElementById("panel-role").classList.contains("hidden"),
       llmVisible: !document.getElementById("panel-llm").classList.contains("hidden"),
     };
   })()`,
);
console.log("切换大模型:", JSON.stringify(llmState));
if (llmState.active !== "llm" || !llmState.roleHidden || !llmState.llmVisible) {
  throw new Error("切换到“大模型设置”失败");
}

// 3. 关键元素均存在（无论面板隐藏与否）
const ids = [
  "passthrough",
  "petdex-url",
  "petdex-import",
  "imported-list",
  "base-url",
  "model",
  "api-key",
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

// 4. 切回“角色设置”，确认已导入角色列表有内容
const roleState = await evaluate(
  settings,
  `(async () => {
     [...document.querySelectorAll(".nav-item")]
       .find((b) => b.dataset.panel === "role")
       .click();
     await new Promise((r) => setTimeout(r, 300));
     return {
       active: document.querySelector(".nav-item.active")?.dataset.panel,
       roleVisible: !document.getElementById("panel-role").classList.contains("hidden"),
       importedCount: document.querySelectorAll("#imported-list .imported-item").length,
     };
   })()`,
);
console.log("切回角色:", JSON.stringify(roleState));
if (roleState.active !== "role" || !roleState.roleVisible || roleState.importedCount === 0) {
  throw new Error("切回“角色设置”或已导入列表异常");
}

persona.close();
settings.close();
console.log("设置侧边栏端到端验证通过 ✓");
