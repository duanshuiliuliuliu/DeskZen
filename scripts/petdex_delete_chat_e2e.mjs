// DeskZen 删除角色 + 聊天窗口冒烟测试（需 CDP 调试端口）
import { writeFile } from "node:fs/promises";
import { cdp, getTargets, evaluate, personaTargetFilter } from "./petdex_e2e_lib.mjs";

const personaTarget = await getTargets(personaTargetFilter);
const persona = cdp(personaTarget.webSocketDebuggerUrl);
await persona.ready;
await persona.send("Runtime.enable");

const listPersonas = () =>
  evaluate(
    persona,
    `window.__TAURI_INTERNALS__.invoke("list_personas")`,
  );

// 1. 准备：确保 maruko 已导入（缺失则先导入）
let ids = (await listPersonas()).map((p) => p.id);
if (!ids.includes("petdex-maruko")) {
  await evaluate(
    persona,
    `window.__TAURI_INTERNALS__.invoke("import_petdex_pet", { url: "https://petdex.dev/pets/maruko" })`,
  );
}

// 2. 删除当前激活角色应自动回退到默认角色
await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("switch_persona", { id: "petdex-maruko" })`,
);
await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("delete_persona", { id: "petdex-maruko" })`,
);
const afterDelete = await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("get_persona_config")`,
);
ids = (await listPersonas()).map((p) => p.id);
console.log("删除后当前角色:", afterDelete.name, "| 角色列表含 maruko:", ids.includes("petdex-maruko"));
if (afterDelete.id !== "shinchan" || ids.includes("petdex-maruko")) {
  throw new Error("删除角色后未正确回退/移除");
}

// 3. 内置角色不可删除
const guardErr = await evaluate(
  persona,
  `window.__TAURI_INTERNALS__.invoke("delete_persona", { id: "shinchan" }).then(
     () => "no-error",
     (e) => String(e),
   )`,
);
console.log("删除内置角色结果:", guardErr);
if (!/内置角色不可删除/.test(guardErr)) throw new Error("内置角色保护未生效");

// 4. 打开聊天窗口并检查新 UI
await evaluate(persona, `window.__TAURI_INTERNALS__.invoke("open_chat")`);
let chatTarget = null;
for (let i = 0; i < 20; i++) {
  await new Promise((r) => setTimeout(r, 500));
  chatTarget = await getTargets((t) => t.type === "page" && t.url.includes("chat"));
  if (chatTarget) break;
}
if (!chatTarget) throw new Error("聊天窗口未打开");

const chat = cdp(chatTarget.webSocketDebuggerUrl);
await chat.ready;
await chat.send("Runtime.enable");
const ui = await evaluate(
  chat,
  `(async () => {
     await new Promise((r) => setTimeout(r, 500));
     return {
       title: document.getElementById("chat-title").textContent,
       avatar: document.getElementById("chat-avatar").textContent,
       state: document.getElementById("chat-state").textContent,
       hasClose: !!document.getElementById("chat-close"),
       hasSend: !!document.querySelector(".chat-send"),
       emptyVisible: getComputedStyle(document.getElementById("chat-empty")).display !== "none",
       rootRadius: getComputedStyle(document.getElementById("chat-root")).borderRadius,
       bodyBg: getComputedStyle(document.body).backgroundColor,
     };
   })()`,
);
console.log("聊天 UI:", JSON.stringify(ui));
if (ui.title !== "蜡笔小新" || !ui.hasClose || !ui.hasSend || !ui.emptyVisible) {
  throw new Error("聊天窗口 UI 异常");
}

// 5. 截图确认视觉效果
const shot = await chat.send("Page.captureScreenshot", { format: "png" });
await writeFile("chat_preview.png", Buffer.from(shot.data, "base64"));
console.log("截图已保存: chat_preview.png");

// 6. 关闭按钮应能关闭窗口
await evaluate(chat, `document.getElementById("chat-close").click()`, false);
let gone = false;
for (let i = 0; i < 20; i++) {
  await new Promise((r) => setTimeout(r, 300));
  const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
  if (!targets.some((t) => t.url.includes("chat"))) {
    gone = true;
    break;
  }
}
console.log("关闭按钮生效:", gone);
if (!gone) throw new Error("聊天窗口关闭失败");

persona.close();
console.log("删除角色 + 聊天窗口端到端验证通过 ✓");
