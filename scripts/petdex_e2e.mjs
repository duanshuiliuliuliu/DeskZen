// DeskZen Petdex 导入端到端冒烟测试（需先以 CDP 调试端口启动应用）：
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222 --remote-allow-origins=*"
//   .\src-tauri\target\release\deskzen.exe
//   node scripts\petdex_e2e.mjs <url>
import { cdp, getTargets, evaluate, personaTargetFilter } from "./petdex_e2e_lib.mjs";

const targetUrl = process.argv[2] ?? "https://petdex.dev/pets/doraemon";

const target = await getTargets(personaTargetFilter);
const client = cdp(target.webSocketDebuggerUrl);
await client.ready;
await client.send("Runtime.enable");

console.log("目标页面:", target.url);

// 1. 执行导入命令
const imported = await evaluate(
  client,
  `window.__TAURI_INTERNALS__.invoke("import_petdex_pet", { url: ${JSON.stringify(targetUrl)} })`,
);
console.log("导入结果:", JSON.stringify(imported));

// 2. 确认当前角色已切换
const persona = await evaluate(
  client,
  `window.__TAURI_INTERNALS__.invoke("get_persona_config")`,
);
console.log("当前角色:", persona.name, "| id:", persona.id);
console.log("精灵图路径:", persona.spritesheet);

// 3. 确认 asset 协议能加载导入的精灵图
const spriteOk = await evaluate(
  client,
  `(async () => {
     const p = await window.__TAURI_INTERNALS__.invoke("get_persona_config");
     const url = window.__TAURI_INTERNALS__.convertFileSrc(p.spritesheet);
     const res = await fetch(url);
     const txt = await res.text().catch(() => "");
     return { ok: res.ok, status: res.status, head: txt.slice(0, 80), url };
   })()`,
);
console.log("精灵图加载:", JSON.stringify(spriteOk));

// 4. 前端应已应用导入角色（背景图切换到 asset URL）
const bg = await evaluate(
  client,
  `document.getElementById("character").style.backgroundImage`,
  false,
);
console.log("角色窗背景:", bg);

client.close();
if (!spriteOk.ok || spriteOk.size < 1000) {
  process.exit(1);
}
console.log("Petdex 导入端到端验证通过 ✓");
