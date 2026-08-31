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

void refresh();
