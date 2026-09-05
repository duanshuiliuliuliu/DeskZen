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

/** 角色精灵在角色窗口内的底距（与 styles.css `.character { bottom: 24px }`、Rust SPRITE_BOTTOM 同步） */
const SPRITE_BOTTOM = 24;
/** 气泡相对精灵头顶的间隙：内置林克 display_h=125 时 bottom=158（24+125+9），此处对齐该几何关系 */
const BUBBLE_GAP = 9;

/** 应用角色外观：精灵图、行数、显示尺寸、渲染模式 */
function setCharSize(w: number, h: number): void {
  character.style.width = `${w}px`;
  character.style.height = `${h}px`;
  character.style.setProperty("--char-w", String(w));
}

/** 根据精灵显示高度把气泡摆到角色头顶上方；未配置显示尺寸时回退到 styles.css 的默认值(158px) */
function applyBubblePosition(h: number): void {
  if (h > 0) {
    bubble.style.bottom = `${SPRITE_BOTTOM + h + BUBBLE_GAP}px`;
  } else {
    // 保留 CSS 默认值（内置林克），避免覆盖造成错位
    bubble.style.removeProperty("bottom");
  }
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
    applyBubblePosition(persona.display_h);
  } else {
    // 未配置显示尺寸：用精灵图实际尺寸 ÷ cols/rows 计算
    const img = new Image();
    img.onload = () => {
      const w = Math.round(img.naturalWidth / persona.cols);
      const h = Math.round(img.naturalHeight / persona.rows);
      setCharSize(w, h);
      applyBubblePosition(h);
    };
    img.onerror = () => {
      setCharSize(1, 1);
      applyBubblePosition(0);
    };
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

  await listen<{ w: number; h: number }>("zoom-changed", (e) => {
    // 缩放变化：直接用事件里的新显示尺寸重排精灵与气泡，无需重新拉取角色配置。
    setCharSize(e.payload.w, e.payload.h);
    applyBubblePosition(e.payload.h);
    if (persona) {
      // 同步本地 persona 的显示尺寸，保证之后再次 applyPersona 也使用缩放后的值。
      persona.display_w = e.payload.w;
      persona.display_h = e.payload.h;
    }
  });

  // 所有事件监听就绪、初始配置也已拉取完毕后，再通知 Rust 广播启动事件
  //（persona-changed / state-changed / 开场气泡），否则广播发生在监听器注册之前会被丢弃。
  await invoke("frontend_ready");
}

void init();
