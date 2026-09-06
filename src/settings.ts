import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import "./styles.css";

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}

const baseUrl = $("base-url") as HTMLInputElement;
const model = $("model") as HTMLInputElement;
const apiKey = $("api-key") as HTMLInputElement;
const temperature = $("temperature") as HTMLInputElement;
const maxTokens = $("max-tokens") as HTMLInputElement;
const aiBubbles = $("ai-bubbles") as HTMLInputElement;
const statusEl = $("settings-status");
const passthrough = $("passthrough") as HTMLInputElement;
const personaZoom = $("persona-zoom") as HTMLSelectElement;
const personaZoomStatus = $("persona-zoom-status") as HTMLElement;
const saveBtn = $("llm-save") as HTMLButtonElement;
const quitBtn = $("quit-app") as HTMLButtonElement;
const petdexUrl = $("petdex-url") as HTMLInputElement;
const petdexImportBtn = $("petdex-import") as HTMLButtonElement;
const petdexStatus = $("petdex-status") as HTMLElement;
const localImportBtn = $("local-import") as HTMLButtonElement;
const localImportZipBtn = $("local-import-zip") as HTMLButtonElement;
const localImportDrop = $("local-import-drop") as HTMLDivElement;
const localImportStatus = $("local-import-status") as HTMLElement;
const importedList = $("imported-list") as HTMLDivElement;
const importedHint = $("imported-hint") as HTMLElement;
const importedStatus = $("imported-status") as HTMLElement;
const navItems = document.querySelectorAll<HTMLButtonElement>(".nav-item");
const settingsContent = document.querySelector(".settings-content") as HTMLElement;
const panels: Record<"role" | "llm" | "about", HTMLElement> = {
  role: $("panel-role"),
  llm: $("panel-llm"),
  about: $("panel-about"),
};

/** Petdex 链接校验：仅接受 https://petdex.dev/pets/{slug} */
const PETDEX_URL_RE = /^https:\/\/petdex\.dev\/pets\/[a-z0-9][a-z0-9-]{0,62}\/?$/i;

/** 角色缩放可选档位：与后端 [0.5, 2.0] 范围一致，步进 25%。 */
const ZOOM_OPTIONS = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/** 在档位里取最接近的值：prefs.json 可能被手改成非档位值（如 1.1），回显时收敛到最近档位。 */
function closestZoom(value: number): number {
  return ZOOM_OPTIONS.reduce(
    (best, opt) => (Math.abs(opt - value) < Math.abs(best - value) ? opt : best),
    ZOOM_OPTIONS[0],
  );
}

function setZoomStatus(text: string, error = false): void {
  personaZoomStatus.textContent = text;
  personaZoomStatus.classList.toggle("error", error);
}

/** 上次 get_llm_config 返回的打码 Key；用于判断输入是否仍是未改动的打码值（仅影响提示文案） */
let loadedMaskedKey = "";

/** 拖放事件订阅句柄，页面卸载时调用以释放（settings 窗口是可复用/关闭后重建的） */
let unlistenDragDrop: (() => void) | undefined;

/** 根据当前输入是否等于已加载的打码值刷新提示文案；用精确比较而非 contains("*")，
 *  避免真实 Key 恰好含 * 时被误判为打码值。 */
function updateMaskHint(): void {
  const masked = loadedMaskedKey !== "" && apiKey.value === loadedMaskedKey;
  apiKey.title = masked
    ? "已保存的 Key 以打码形式显示；重新输入完整 Key 可替换"
    : "";
  apiKey.placeholder = masked ? "" : "sk-...";
}

apiKey.addEventListener("input", updateMaskHint);

function setPetdexStatus(text: string, error = false): void {
  petdexStatus.textContent = text;
  petdexStatus.classList.toggle("error", error);
}

function setLocalImportStatus(text: string, error = false): void {
  localImportStatus.textContent = text;
  localImportStatus.classList.toggle("error", error);
}

/** 角色列表区状态提示（设为当前 / 删除结果） */
function setImportedStatus(text: string, error = false): void {
  importedStatus.textContent = text;
  importedStatus.classList.toggle("error", error);
}

/** 滚动到指定面板并高亮侧边栏 */
function activatePanel(name: "role" | "llm" | "about", scroll = true): void {
  for (const btn of navItems) {
    btn.classList.toggle("active", btn.dataset.panel === name);
  }
  if (scroll) {
    panels[name].scrollIntoView({ behavior: "smooth", block: "start" });
  }
}

for (const btn of navItems) {
  btn.addEventListener("click", () => {
    const name = btn.dataset.panel;
    if (name === "role" || name === "llm" || name === "about") activatePanel(name);
  });
}

// 滚动时高亮当前面板（内容滚动会吸附到每个设置区块）
settingsContent.addEventListener("scroll", () => {
  const rectTop = settingsContent.getBoundingClientRect().top;
  let current: "role" | "llm" | "about" = "role";
  let best = -Infinity;
  for (const name of ["role", "llm", "about"] as const) {
    const d = panels[name].getBoundingClientRect().top - rectTop;
    if (d <= 80 && d > best) {
      best = d;
      current = name;
    }
  }
  for (const btn of navItems) {
    btn.classList.toggle("active", btn.dataset.panel === current);
  }
});

/** 展示全部角色（内置 + 导入），高亮当前行；导入角色提供「设为当前」「删除」 */
async function refreshImported(): Promise<void> {
  const personas = await invoke<{ id: string; name: string }[]>("list_personas");
  const currentId = await invoke<string>("get_current_persona_id");
  importedList.textContent = "";
  if (personas.length === 0) {
    importedHint.textContent = "暂无导入角色，可在下方导入";
    return;
  }
  importedHint.textContent = "";
  for (const p of personas) {
    const isImported = p.id.startsWith("petdex-") || p.id.startsWith("local-");
    const isCurrent = p.id === currentId;
    const row = document.createElement("div");
    row.className = `imported-item${isCurrent ? " active" : ""}`;

    const name = document.createElement("span");
    name.className = "imported-name";
    name.textContent = p.name;

    const actions = document.createElement("span");
    actions.className = "imported-actions";

    // 当前角色加「当前」徽标
    if (isCurrent) {
      const badge = document.createElement("span");
      badge.className = "imported-current";
      badge.textContent = "当前";
      actions.appendChild(badge);
    }

    // 所有角色（内置 + 导入）都可设为当前，避免切到导入角色后无法回到内置角色；当前角色按钮禁用
    const use = document.createElement("button");
    use.type = "button";
    use.className = "imported-use";
    use.textContent = "设为当前";
    use.disabled = isCurrent;
    use.addEventListener("click", async () => {
      use.disabled = true;
      try {
        await invoke("switch_persona", { id: p.id });
        // 成功时不显示提示：角色列表高亮本身就是反馈
      } catch (err) {
        setImportedStatus(`切换失败：${String(err)}`, true);
      } finally {
        await refreshImported();
      }
    });
    actions.appendChild(use);

    // 仅导入角色可删除（内置角色不可删除）
    if (isImported) {
      const del = document.createElement("button");
      del.type = "button";
      del.className = "imported-del";
      del.textContent = "删除";
      del.addEventListener("click", async () => {
        del.disabled = true;
        try {
          await invoke("delete_persona", { id: p.id });
          setImportedStatus(`已删除角色：${p.name}`);
        } catch (err) {
          setImportedStatus(`删除失败：${String(err)}`, true);
        } finally {
          await refreshImported();
        }
      });
      actions.appendChild(del);
    }

    row.append(name, actions);
    importedList.appendChild(row);
  }
}

async function refresh(): Promise<void> {
  const cfg = await invoke<{
    base_url: string;
    model: string;
    api_key: string;
    temperature: number;
    max_tokens: number;
  }>("get_llm_config");
  baseUrl.value = cfg.base_url;
  model.value = cfg.model;
  apiKey.value = cfg.api_key;
  loadedMaskedKey = cfg.api_key;
  temperature.value = String(cfg.temperature);
  maxTokens.value = String(cfg.max_tokens);
  updateMaskHint();
  passthrough.checked = await invoke<boolean>("get_passthrough");
  const prefs = await invoke<{ zoom: number; ai_bubbles: boolean }>("get_prefs");
  personaZoom.value = String(closestZoom(prefs.zoom));
  aiBubbles.checked = prefs.ai_bubbles;
}

saveBtn.addEventListener("click", async () => {
  statusEl.textContent = "保存中…";
  try {
    await invoke("save_llm_config", {
      baseUrl: baseUrl.value.trim(),
      model: model.value.trim(),
      apiKey: apiKey.value.trim(),
      temperature: temperature.value.trim() === "" ? 0.8 : Number(temperature.value),
      maxTokens: maxTokens.value.trim() === "" ? 512 : Number(maxTokens.value),
    });
    statusEl.textContent = "已保存";
  } catch (err) {
    statusEl.textContent = `保存失败：${String(err)}`;
  }
});

passthrough.addEventListener("change", () => {
  void invoke("set_passthrough", { enabled: passthrough.checked });
});

aiBubbles.addEventListener("change", async () => {
  try {
    await invoke("set_ai_bubbles", { enabled: aiBubbles.checked });
    statusEl.textContent = aiBubbles.checked
      ? "已开启 AI 每日气泡"
      : "已关闭 AI 每日气泡";
  } catch (err) {
    statusEl.textContent = `设置失败：${String(err)}`;
    aiBubbles.checked = !aiBubbles.checked;
  }
});

personaZoom.addEventListener("change", async () => {
  const zoom = parseFloat(personaZoom.value);
  try {
    await invoke("set_zoom", { zoom });
    setZoomStatus(`已缩放至 ${Math.round(zoom * 100)}%`);
  } catch (err) {
    setZoomStatus(`缩放设置失败：${String(err)}`, true);
  }
});

quitBtn.addEventListener("click", () => {
  void invoke("quit_app");
});

petdexImportBtn.addEventListener("click", async () => {
  const url = petdexUrl.value.trim();
  if (!url) {
    setPetdexStatus("请输入 Petdex 角色链接", true);
    petdexUrl.focus();
    return;
  }
  if (!PETDEX_URL_RE.test(url)) {
    setPetdexStatus("链接格式不正确，示例：https://petdex.dev/pets/doraemon", true);
    return;
  }
  petdexImportBtn.disabled = true;
  setPetdexStatus("正在下载并导入…");
  try {
    const pet = await invoke<{ id: string; name: string }>("import_petdex_pet", {
      url,
    });
    setPetdexStatus(`导入成功：${pet.name}（已切换）`);
    await refreshImported();
  } catch (err) {
    setPetdexStatus(`导入失败：${String(err)}`, true);
  } finally {
    petdexImportBtn.disabled = false;
  }
});

/** 从本地 zip / 文件夹导入 */
async function importFromPath(
  path: string,
  pendingMessage = "正在导入…",
): Promise<void> {
  localImportBtn.disabled = true;
  setLocalImportStatus(pendingMessage);
  try {
    const pet = await invoke<{ id: string; name: string }>("import_local_character", {
      path,
    });
    setLocalImportStatus(`导入成功：${pet.name}（已切换）`);
    await refreshImported();
  } catch (err) {
    setLocalImportStatus(`导入失败：${String(err)}`, true);
  } finally {
    localImportBtn.disabled = false;
  }
}

localImportBtn.addEventListener("click", async () => {
  const path = await open({ directory: true, multiple: false });
  if (typeof path === "string") await importFromPath(path);
});

localImportZipBtn.addEventListener("click", async () => {
  const path = await open({
    multiple: false,
    filters: [{ name: "角色资源包", extensions: ["zip"] }],
  });
  if (typeof path === "string") await importFromPath(path);
});

// 用 Tauri 的拖放事件而非 HTML5 drop：系统拖拽会被 WebView 拦截，HTML5 拿不到路径。
void getCurrentWebview()
  .onDragDropEvent((event) => {
    const { type } = event.payload;
    if (type === "enter" || type === "over") {
      // 拖入中：高亮提示“松手即导入”
      localImportDrop.classList.add("drag-over");
      return;
    }
    localImportDrop.classList.remove("drag-over");
    if (type === "drop") {
      // 拖入多文件时只取第一个，避免误导入；路径为空则忽略
      const path = event.payload.paths[0];
      if (!path) return;
      const fileName = path.split(/[\\/]/).pop() || path;
      void importFromPath(path, `正在导入：${fileName}…`);
    }
  })
  .then((unlisten) => {
    unlistenDragDrop = unlisten;
  })
  .catch(() => {
    // 监听注册失败时静默降级：拖拽入口不可用，但两个按钮仍可导入，不影响其他功能。
  });

// 页面卸载时释放拖放监听，避免窗口关闭/重建后残留监听器
window.addEventListener("pagehide", () => unlistenDragDrop?.());

// 外部切换角色（托盘“更换角色”等）后同步列表高亮；忽略完整 persona 负载，仅刷新即可
void listen("persona-changed", () => {
  setImportedStatus(""); // 清空旧提示，避免残留「已切换至」误导
  void refreshImported();
});

void refresh();
void refreshImported();

// 等前端就绪再显示设置窗，避免 WebView2 未渲染时闪现空白窗口（白屏闪烁）。
void getCurrentWindow().show();
void getCurrentWindow().setFocus();
