import { invoke } from "@tauri-apps/api/core";
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
const importedList = $("imported-list") as HTMLDivElement;
const importedHint = $("imported-hint") as HTMLElement;
const navItems = document.querySelectorAll<HTMLButtonElement>(".nav-item");
const rolePanel = $("panel-role") as HTMLElement;
const llmPanel = $("panel-llm") as HTMLElement;

/** Petdex 链接校验：仅接受 https://petdex.dev/pets/{slug} */
const PETDEX_URL_RE = /^https:\/\/petdex\.dev\/pets\/[a-z0-9][a-z0-9-]{0,62}\/?$/i;

function setPetdexStatus(text: string, error = false): void {
  petdexStatus.textContent = text;
  petdexStatus.classList.toggle("error", error);
}

/** 侧边栏切换：角色设置 / 大模型设置 */
function switchPanel(name: "role" | "llm"): void {
  for (const btn of navItems) {
    btn.classList.toggle("active", btn.dataset.panel === name);
  }
  rolePanel.classList.toggle("hidden", name !== "role");
  llmPanel.classList.toggle("hidden", name !== "llm");
}

for (const btn of navItems) {
  btn.addEventListener("click", () => {
    const name = btn.dataset.panel;
    if (name === "role" || name === "llm") switchPanel(name);
  });
}

/** 展示已导入（petdex- 前缀）的角色列表 */
async function refreshImported(): Promise<void> {
  const personas = await invoke<{ id: string; name: string }[]>("list_personas");
  const imported = personas.filter((p) => p.id.startsWith("petdex-"));
  importedList.textContent = "";
  if (imported.length === 0) {
    importedHint.textContent = "还没有导入角色，粘贴 Petdex 链接即可添加";
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
  const cfg = await invoke<{ base_url: string; model: string; api_key: string }>(
    "get_llm_config",
  );
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

void refresh();
void refreshImported();
