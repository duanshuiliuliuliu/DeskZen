// DeskZen Petdex 导入 UI 冒烟测试（配合 petdex_e2e.mjs 使用，同样需要 CDP 调试端口）
import { cdp, getTargets, evaluate, personaTargetFilter } from "./petdex_e2e_lib.mjs";

const personaTarget = await getTargets(personaTargetFilter);
const persona = cdp(personaTarget.webSocketDebuggerUrl);
await persona.ready;
await persona.send("Runtime.enable");

// 1. 重启后导入的角色应仍在（引擎启动时扫描用户数据目录）
await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("switch_persona", { id: "petdex-doraemon" })`,
);
const active = await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("get_persona_config")`,
);
console.log("重启后切换:", active.name, "|", active.id);
if (active.id !== "petdex-doraemon") throw new Error("重启后角色未持久化");

// 2. 打开设置窗口
await evaluate(persona, `window.__TAURI_INTERNALS__.invoke("open_settings")`, true);

let settingsTarget = null;
for (let i = 0; i < 20; i++) {
  await new Promise((r) => setTimeout(r, 500));
  settingsTarget = await getTargets((t) => t.type === "page" && t.url.includes("settings"));
  if (settingsTarget) break;
}
if (!settingsTarget) throw new Error("设置窗口未打开");
console.log("设置窗口:", settingsTarget.url);

const settings = cdp(settingsTarget.webSocketDebuggerUrl);
await settings.ready;
await settings.send("Runtime.enable");

// 3. 前端校验：非法链接应提示错误，不触发后端
const invalidResult = await evaluate(
  settings,
  `(async () => {
     const input = document.getElementById("petdex-url");
     input.value = "https://evil.example.com/pets/x";
     document.getElementById("petdex-import").click();
     await new Promise((r) => setTimeout(r, 500));
     return {
       hasInput: !!input,
       status: document.getElementById("petdex-status").textContent,
     };
   })()`,
);
console.log("非法链接:", JSON.stringify(invalidResult));
if (!invalidResult.hasInput || !/格式不正确/.test(invalidResult.status)) {
  throw new Error("前端校验未生效");
}

// 4. 合法链接：导入 maruko 并等待成功
await evaluate(
  settings,
  `(async () => {
     const input = document.getElementById("petdex-url");
     input.value = "https://petdex.dev/pets/maruko";
     document.getElementById("petdex-import").click();
   })()`,
);

let importStatus = "";
for (let i = 0; i < 120; i++) {
  await new Promise((r) => setTimeout(r, 1000));
  importStatus = await evaluate(
    settings,
    `document.getElementById("petdex-status").textContent`,
    false,
  );
  if (/导入成功|导入失败/.test(importStatus)) break;
}
console.log("导入 maruko 状态:", importStatus);
if (!/导入成功/.test(importStatus)) throw new Error("合法链接导入未成功");

// 5. 角色窗应已切换到 Maruko 且精灵图可加载
const maruko = await evaluate(
  persona,
  `(async () => {
     const p = await window.__TAURI_INTERNALS__.invoke("get_persona_config");
     const url = window.__TAURI_INTERNALS__.convertFileSrc(p.spritesheet);
     const res = await fetch(url);
     return { name: p.name, id: p.id, ok: res.ok, status: res.status };
   })()`,
);
console.log("Maruko 精灵图:", JSON.stringify(maruko));
if (maruko.id !== "petdex-maruko" || !maruko.ok) {
  throw new Error("Maruko 切换或精灵图加载失败");
}

persona.close();
settings.close();
console.log("Petdex 导入 UI 端到端验证通过 ✓");
