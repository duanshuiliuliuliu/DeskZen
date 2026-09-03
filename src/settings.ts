import { invoke } from "@tauri-apps/api/core";
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
const statusEl = $("settings-status");
const passthrough = $("passthrough") as HTMLInputElement;
const saveBtn = $("llm-save") as HTMLButtonElement;
const quitBtn = $("quit-app") as HTMLButtonElement;
const petdexUrl = $("petdex-url") as HTMLInputElement;
const petdexImportBtn = $("petdex-import") as HTMLButtonElement;
const petdexStatus = $("petdex-status") as HTMLElement;
const localImportBtn = $("local-import") as HTMLButtonElement;
const localImportStatus = $("local-import-status") as HTMLElement;
const importedList = $("imported-list") as HTMLDivElement;
const importedHint = $("imported-hint") as HTMLElement;
const navItems = document.querySelectorAll<HTMLButtonElement>(".nav-item");
const settingsContent = document.querySelector(".settings-content") as HTMLElement;
const panels: Record<"role" | "llm" | "about", HTMLElement> = {
  role: $("panel-role"),
  llm: $("panel-llm"),
  about: $("panel-about"),
};

/** Petdex 链接校验：仅接受 https://petdex.dev/pets/{slug} */
const PETDEX_URL_RE = /^https:\/\/petdex\.dev\/pets\/[a-z0-9][a-z0-9-]{0,62}\/?$/i;

function setPetdexStatus(text: string, error = false): void {
  petdexStatus.textContent = text;
  petdexStatus.classList.toggle("error", error);
}

function setLocalImportStatus(text: string, error = false): void {
  localImportStatus.textContent = text;
  localImportStatus.classList.toggle("error", error);
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

/** 展示已导入（petdex- / local- 前缀）的角色列表 */
async function refreshImported(): Promise<void> {
  const personas = await invoke<{ id: string; name: string }[]>("list_personas");
  const imported = personas.filter(
    (p) => p.id.startsWith("petdex-") || p.id.startsWith("local-"),
  );
  importedList.textContent = "";
  if (imported.length === 0) {
    importedHint.textContent = "还没有导入角色，可粘贴 Petdex 链接或从本地导入";
    return;
  }
  importedHint.textContent = "";
  for (const p of imported) {
    const row = document.createElement("div");
    row.className = "imported-item";
    const name = document.createElement("span");
    name.className = "imported-name";
    name.textContent = p.name;
    const del = document.createElement("button");
    del.type = "button";
    del.className = "imported-del";
    del.textContent = "删除";
    del.addEventListener("click", async () => {
      del.disabled = true;
      try {
        await invoke("delete_persona", { id: p.id });
        setPetdexStatus(`已删除角色：${p.name}`);
      } catch (err) {
        setPetdexStatus(`删除失败：${String(err)}`, true);
      } finally {
        await refreshImported();
      }
    });
    row.append(name, del);
    importedList.appendChild(row);
  }
}

async function refresh(): Promise<void> {
  const cfg = await invoke<{
    base_url: string;
    model: string;
    api_key: string;
  }>("get_llm_config");
  baseUrl.value = cfg.base_url;
  model.value = cfg.model;
  apiKey.value = cfg.api_key;
  passthrough.checked = await invoke<boolean>("get_passthrough");
}

saveBtn.addEventListener("click", async () => {
  statusEl.textContent = "保存中…";
  try {
    await invoke("save_llm_config", {
      baseUrl: baseUrl.value.trim(),
      model: model.value.trim(),
      apiKey: apiKey.value.trim(),
    });
    statusEl.textContent = "已保存";
  } catch (err) {
    statusEl.textContent = `保存失败：${String(err)}`;
  }
});

passthrough.addEventListener("change", () => {
  void invoke("set_passthrough", { enabled: passthrough.checked });
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
async function importFromPath(path: string): Promise<void> {
  localImportBtn.disabled = true;
  setLocalImportStatus("正在导入…");
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

void refresh();
void refreshImported();
