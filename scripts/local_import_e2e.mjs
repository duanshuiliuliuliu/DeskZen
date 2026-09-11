// DeskZen 本地角色导入 / 删除 + 聊天窗口冒烟测试（需以 CDP 调试端口启动应用）
//
// 用法：先带 CDP 端口启动应用，再运行
//   node scripts/local_import_e2e.mjs
//
// 脚本会在系统临时目录拼一个最小角色包（persona.json + clips/observe.webp），
// 走 import_local_character 导入 → 校验切换与前端渲染 → 删除并校验回退，
// 全程不联网、不污染仓库。
import { cp, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { cdp, getTargets, evaluate, personaTargetFilter } from "./cdp_e2e_lib.mjs";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));

const PACK_PERSONA = {
  id: "demo",
  name: "本地演示角色",
  display_w: 96,
  display_h: 104,
  system_prompt: {
    definition: "用于冒烟测试的演示角色",
    reply_style: "简短回复",
    state_guidelines: { idle: "你正在待机" },
  },
  states: {
    idle: { label: "待机", bubbles: ["测试中。"] },
  },
  clips: {
    observe: { spritesheet: "clips/observe.webp", frames: 60, frame_ms: 83 },
  },
  scenes: {
    idle: [{ id: "look_around", label: "观察周围", steps: [{ clip: "observe" }] }],
  },
  schedule: { loop: [{ state: "idle", duration: 10 }], time: [] },
};

const personaTarget = await getTargets(personaTargetFilter);
const persona = cdp(personaTarget.webSocketDebuggerUrl);
await persona.ready;
await persona.send("Runtime.enable");

const invoke = (method, args = {}) =>
  evaluate(
    persona,
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(method)}, ${JSON.stringify(args)})`,
  );

const listIds = async () => (await invoke("list_personas")).map((p) => p.id);

const packRoot = await mkdtemp(join(tmpdir(), "deskzen-local-pack-"));
await mkdir(join(packRoot, "clips"), { recursive: true });
await writeFile(join(packRoot, "persona.json"), JSON.stringify(PACK_PERSONA, null, 2), "utf8");
await cp(
  join(repoRoot, "resources/characters/link/clips/observe.webp"),
  join(packRoot, "clips/observe.webp"),
);

try {
  // 1. 导入最小角色包
  const imported = await invoke("import_local_character", { path: packRoot });
  console.log("导入结果:", JSON.stringify(imported));
  if (imported.id !== "local-demo" || imported.name !== "本地演示角色") {
    throw new Error("本地导入返回的角色 id / 名称不符合预期");
  }

  // 2. 导入即切换，且角色列表包含新角色
  const currentId = await invoke("get_current_persona_id");
  if (currentId !== imported.id) throw new Error(`导入后未切换：当前 ${currentId}`);
  if (!(await listIds()).includes(imported.id)) throw new Error("角色列表缺少导入角色");

  // 3. 前端确实在播放新角色的动作片段（applyClip 会写 dataset.clip）
  let clip = null;
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 300));
    clip = await evaluate(persona, `document.getElementById("character").dataset.clip ?? ""`);
    if (clip === "observe") break;
  }
  console.log("当前播放动作:", clip || "(无)");
  if (clip !== "observe") throw new Error("角色窗口未播放导入角色的动作片段");

  // 3b. 帧条确实能解码（资源路径经 asset 协议可读）
  const rendered = await evaluate(
    persona,
    `(async () => {
       const el = document.getElementById("character");
       const url = getComputedStyle(el).backgroundImage.replace(/^url\\(["']?/, "").replace(/["']?\\)$/, "");
       const ok = await new Promise((resolve) => {
         const img = new Image();
         img.onload = () => resolve(img.naturalWidth > 0);
         img.onerror = () => resolve(false);
         img.src = url;
       });
       return { url, ok };
     })()`,
  );
  console.log("帧条加载:", JSON.stringify(rendered));
  if (!rendered.ok) throw new Error(`动作帧条无法加载: ${rendered.url}`);

  // 4. 删除当前角色应自动回退到内置角色
  await invoke("delete_persona", { id: imported.id });
  const afterDelete = await invoke("get_persona_config");
  if (afterDelete.id !== "link" || (await listIds()).includes(imported.id)) {
    throw new Error("删除角色后未正确回退/移除");
  }

  // 5. 内置角色不可删除
  const guardError = await evaluate(
    persona,
    `window.__TAURI_INTERNALS__.invoke("delete_persona", { id: "link" }).then(
       () => "no-error",
       (e) => String(e),
     )`,
  );
  console.log("删除内置角色结果:", guardError);
  if (!/内置角色不可删除/.test(guardError)) throw new Error("内置角色保护未生效");

  // 6. 聊天窗口 UI 仍可用
  await invoke("open_chat");
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
         hasClose: !!document.getElementById("chat-close"),
         hasSend: !!document.querySelector(".chat-send"),
       };
     })()`,
  );
  console.log("聊天 UI:", JSON.stringify(ui));
  if (ui.title !== "林克" || !ui.hasClose || !ui.hasSend) {
    throw new Error("聊天窗口 UI 异常");
  }
  await evaluate(chat, `document.getElementById("chat-close").click()`, false);
  chat.close();

  console.log("本地导入 / 删除 + 聊天窗口端到端验证通过 ✓");
} finally {
  await rm(packRoot, { recursive: true, force: true });
  persona.close();
}
