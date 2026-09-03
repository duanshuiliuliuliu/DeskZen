import { convertFileSrc, invoke } from "@tauri-apps/api/core";
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
  spritesheet: string;
  cols: number;
  rows: number;
  pixel_art: boolean;
  display_w: number;
  display_h: number;
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
let bubbleTimer: ReturnType<typeof setTimeout> | undefined;
let dragging = false;
let pointerStart = { x: 0, y: 0 };

/** 应用角色外观：精灵图、行数、显示尺寸、渲染模式 */
function setCharSize(w: number, h: number): void {
  character.style.width = `${w}px`;
  character.style.height = `${h}px`;
  character.style.setProperty("--char-w", String(w));
}

function applyPersona(persona: PersonaConfig): void {
  document.body.style.setProperty("--cols", String(persona.cols));
  document.body.style.setProperty("--rows", String(persona.rows));
  // 内置角色用站点路径；petdex 导入角色存在磁盘上，需经 asset 协议加载
  const spriteUrl = persona.spritesheet.startsWith("/")
    ? persona.spritesheet
    : convertFileSrc(persona.spritesheet);
  character.style.backgroundImage = `url("${spriteUrl}")`;
  character.style.imageRendering = persona.pixel_art ? "pixelated" : "auto";
  if (persona.display_w > 0 && persona.display_h > 0) {
    setCharSize(persona.display_w, persona.display_h);
  } else {
    // 未配置显示尺寸：用精灵图实际尺寸 ÷ cols/rows 计算
    const img = new Image();
    img.onload = () => {
      const w = Math.round(img.naturalWidth / persona.cols);
      const h = Math.round(img.naturalHeight / persona.rows);
      setCharSize(w, h);
    };
    img.onerror = () => setCharSize(1, 1);
    img.src = spriteUrl;
  }
}

/** 切换角色状态：CSS 变量驱动 spritesheet 的行列位置与帧动画参数 */
function applyState(state: string, cfg: StateConfig | undefined): void {
  document.body.dataset.state = state;
  if (!cfg) return;
  document.title = persona ? `DeskZen · ${persona.name} · ${cfg.label}` : "DeskZen";
  document.body.style.setProperty("--frames", String(cfg.frames));
  document.body.style.setProperty("--row", String(cfg.row));
  const rows = persona?.rows ?? 3;
  const rowDivisor = Math.max(rows - 1, 1);
  character.style.backgroundPositionY = `${(cfg.row / rowDivisor) * 100}%`;
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
  // 右键角色 → 弹出原生菜单（下个状态 / 隐藏）
  character.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    void invoke("show_persona_menu");
  });
  // 禁用 WebView2 默认右键菜单（其余区域右键不做任何事）
  document.addEventListener("contextmenu", (e) => e.preventDefault());

  persona = await invoke<PersonaConfig>("get_persona_config");
  const state = await invoke<string>("get_current_state");
  applyPersona(persona);
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

  await listen<PersonaConfig>("persona-changed", (e) => {
    persona = e.payload;
    applyPersona(persona);
    // 切换后引擎会紧接着广播 state-changed，这里只刷新标题
    const cfg = persona.states[document.body.dataset.state ?? ""];
    document.title = `DeskZen · ${persona.name} · ${cfg?.label ?? ""}`;
  });

  await listen<BubblePayload>("bubble", (e) => {
    showBubble(e.payload.text);
  });
}

void init();
