import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

interface StateConfig {
  label: string;
  row: number;
  frames: number;
  frame_ms: number;
  bubbles: string[];
}

interface PersonaConfig {
  id: string;
  name: string;
  states: Record<string, StateConfig>;
}

interface StateChangedPayload {
  state: string;
  previous: string;
}

interface BubblePayload {
  state: string;
  text: string;
}

let persona: PersonaConfig | null = null;

const character = document.getElementById("character") as HTMLDivElement;
const bubble = document.getElementById("bubble") as HTMLDivElement;
const stateTag = document.getElementById("state-tag") as HTMLDivElement;
let bubbleTimer: ReturnType<typeof setTimeout> | undefined;
let dragging = false;
let pointerStart = { x: 0, y: 0 };

/** 切换角色状态：CSS 变量驱动 spritesheet 的行列位置与帧动画参数 */
function applyState(state: string, cfg: StateConfig | undefined): void {
  document.body.dataset.state = state;
  if (!cfg) return;
  stateTag.textContent = cfg.label;
  document.body.style.setProperty("--cols", String(cfg.frames));
  document.body.style.setProperty("--row", String(cfg.row));
  character.style.animationName = cfg.frames > 1 ? "sprite-cycle" : "none";
  character.style.animationDuration = `${cfg.frames * cfg.frame_ms}ms`;
}

function showBubble(text: string): void {
  bubble.textContent = text;
  bubble.classList.remove("hidden");
  requestAnimationFrame(() => bubble.classList.add("show"));
  if (bubbleTimer) clearTimeout(bubbleTimer);
  bubbleTimer = setTimeout(() => {
    bubble.classList.remove("show");
    bubble.classList.add("hidden");
  }, 6000);
}

async function init(): Promise<void> {
  // 禁用 WebView2 默认右键菜单（右键点击角色不做任何事）
  document.addEventListener("contextmenu", (e) => e.preventDefault());

  persona = await invoke<PersonaConfig>("get_persona_config");
  const state = await invoke<string>("get_current_state");
  document.title = `DeskZen · ${persona.name} · ${state}`;
  applyState(state, persona.states[state]);

  // 按住角色拖动窗口：移动超过阈值才进入原生拖动，否则视为点击
  character.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    dragging = false;
    pointerStart = { x: e.screenX, y: e.screenY };
  });

  character.addEventListener("pointermove", (e) => {
    if (e.buttons !== 1 || dragging) return;
    const dx = e.screenX - pointerStart.x;
    const dy = e.screenY - pointerStart.y;
    if (Math.abs(dx) > 4 || Math.abs(dy) > 4) {
      dragging = true;
      void getCurrentWindow().startDragging();
    }
  });

  // 左键双击（未拖动）角色 → 打开对话窗口
  character.addEventListener("dblclick", () => {
    if (dragging) {
      dragging = false;
      return;
    }
    void invoke("open_chat");
  });

  await listen<StateChangedPayload>("state-changed", (e) => {
    if (!persona) return;
    applyState(e.payload.state, persona.states[e.payload.state]);
  });

  await listen<BubblePayload>("bubble", (e) => {
    showBubble(e.payload.text);
  });
}

void init();
