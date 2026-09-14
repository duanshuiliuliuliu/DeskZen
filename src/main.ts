import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

interface StateConfig {
  label: string;
}

/** 后端下发的角色视图：只有尺寸与状态名；动作参数随 playback 事件一起到达 */
interface PersonaConfig {
  id: string;
  name: string;
  display_w: number;
  display_h: number;
  states: Record<string, StateConfig>;
}

interface StateChangedPayload {
  state: string;
}

interface BubblePayload {
  text: string;
  /** 展示时长（毫秒）：后端按当前动作剩余时间给出，保证气泡不跨动作 */
  show_ms: number;
}

/** 后端下发的播放指令：前端只负责渲染，不再自己挑场景 */
interface PlaybackPayload {
  state: string;
  scene_id: string;
  scene_label: string;
  step_index: number;
  clip: string;
  spritesheet: string;
  frames: number;
  frame_ms: number;
  loops: number;
  duration_ms: number;
}

let persona: PersonaConfig | null = null;

const character = document.getElementById("character") as HTMLDivElement;
const layers = Array.from(
  character.querySelectorAll<HTMLDivElement>(".character-layer"),
);
const bubble = document.getElementById("bubble") as HTMLDivElement;
let bubbleTimer: ReturnType<typeof setTimeout> | undefined;
/** 当前显示的是哪一层（换动作时把新动作画到另一层并交叉淡入） */
let activeLayer = 0;
let dragging = false;
let pointerStart = { x: 0, y: 0 };

/** 交叉淡入时长（ms）：写进 CSS 变量 --fade-ms，样式与这里的清理定时器同源 */
const FADE_MS = 150;
document.documentElement.style.setProperty("--fade-ms", `${FADE_MS}ms`);
/** 角色精灵在角色窗口内的底距（与 styles.css `.character { bottom: 24px }`、Rust SPRITE_BOTTOM 同步） */
const SPRITE_BOTTOM = 24;
/** 气泡相对精灵头顶的间隙：内置林克 display_h=125 时 bottom=158（24+125+9），此处对齐该几何关系 */
const BUBBLE_GAP = 9;

/** 设置角色元素尺寸（px），并同步给 CSS 关键帧使用的 --char-w */
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

function resolveSpriteUrl(path: string): string {
  return path.startsWith("/") ? path : convertFileSrc(path);
}

/** 应用角色外观尺寸（后端已把 display 尺寸算好并乘过全局缩放） */
function applyPersona(persona: PersonaConfig): void {
  const w = Math.max(1, persona.display_w);
  const h = Math.max(1, persona.display_h);
  setCharSize(w, h);
  applyBubblePosition(h);
}

/** 在指定图层上准备一个动作：设置帧参数、贴图并强制从第 1 帧重新播放 */
function prepareLayer(layer: HTMLDivElement, payload: PlaybackPayload): void {
  const frames = Math.max(1, Math.floor(payload.frames));
  const frameMs = Math.max(40, Math.floor(payload.frame_ms));
  layer.style.setProperty("--cols", String(frames));
  layer.style.setProperty("--rows", "1");
  layer.style.setProperty("--frames", String(frames));
  layer.style.backgroundImage = `url("${resolveSpriteUrl(payload.spritesheet)}")`;
  layer.style.animationName = "none";
  // 强制重排后再挂动画，保证换动作时从第 1 帧开始（而不是延续上一个动作的进度）
  void layer.offsetWidth;
  layer.style.animationDuration = `${frames * frameMs}ms`;
  layer.style.animationName = frames > 1 ? "sprite-cycle" : "none";
}

/** 清空所有图层（切换角色 / 无可播配置时用） */
function clearLayers(): void {
  for (const layer of layers) {
    layer.style.animationName = "none";
    layer.style.backgroundImage = "none";
  }
  delete character.dataset.scene;
  delete character.dataset.clip;
}

/**
 * 播放后端下发的动作：新动作用另一个图层渲染并交叉淡入，旧图层随后停掉。
 * 硬切会暴露动作间的姿态/尺寸差异，淡入淡出能显著提升衔接的自然度。
 */
function playClip(payload: PlaybackPayload): void {
  const incoming = layers[1 - activeLayer];
  const outgoing = layers[activeLayer];
  prepareLayer(incoming, payload);
  incoming.style.zIndex = "2";
  outgoing.style.zIndex = "1";
  incoming.style.opacity = "0";
  requestAnimationFrame(() => {
    incoming.style.opacity = "1";
    outgoing.style.opacity = "0";
  });
  const previous = outgoing;
  window.setTimeout(() => {
    // 淡出结束后停掉旧图层，避免两个无限动画白白占 CPU
    if (previous !== layers[activeLayer]) previous.style.animationName = "none";
  }, FADE_MS + 50);
  activeLayer = 1 - activeLayer;

  // 测试钩子与标题；气泡属于上一个动作，画面换了就作废
  character.dataset.clip = payload.clip;
  character.dataset.scene = payload.scene_id;
  document.title = persona
    ? `DeskZen · ${persona.name} · ${payload.scene_label || payload.state}`
    : "DeskZen";
  hideBubble();
}

/** 切换角色状态：只记状态，具体播什么等后端的 playback 指令 */
function applyState(state: string, cfg: StateConfig | undefined): void {
  document.body.dataset.state = state;
  if (!cfg) return;
  if (!character.dataset.clip) {
    document.title = persona ? `DeskZen · ${persona.name} · ${cfg.label}` : "DeskZen";
  }
}

function showBubble(text: string, showMs = 6000): void {
  bubble.textContent = text;
  bubble.classList.remove("hidden");
  requestAnimationFrame(() => bubble.classList.add("show"));
  if (bubbleTimer) clearTimeout(bubbleTimer);
  bubbleTimer = setTimeout(() => {
    bubble.classList.remove("show");
    bubble.classList.add("hidden");
  }, Math.max(500, showMs));
}

/** 收起当前气泡：动作已经切换，这条文案不再对应当前画面 */
function hideBubble(): void {
  if (bubbleTimer) clearTimeout(bubbleTimer);
  bubbleTimer = undefined;
  bubble.classList.remove("show");
  bubble.classList.add("hidden");
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

  // 用户"注意到角色"：鼠标凑近 / 点一下（未拖动）→ 让角色放下手上的事看你一眼，
  // 反应结束再回到原来在做的事（限频在后端，鼠标蹭过窗口不会反复打断）
  character.addEventListener("pointerenter", () => {
    void invoke("notify_seen", { kind: "hover" });
  });

  character.addEventListener("pointerup", (e) => {
    if (e.button !== 0) return;
    if (dragging) {
      dragging = false; // 拖动收尾，不算"点一下"
      return;
    }
    void invoke("notify_seen", { kind: "click" });
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

  // 后端决定播什么：前端只负责渲染 + 交叉淡入
  await listen<PlaybackPayload>("playback", (e) => {
    playClip(e.payload);
  });

  await listen<PersonaConfig>("persona-changed", (e) => {
    hideBubble();
    clearLayers();
    persona = e.payload;
    applyPersona(persona);
    // 切换后引擎会紧接着广播 state-changed / playback，这里只刷新标题
    const cfg = persona.states[document.body.dataset.state ?? ""];
    document.title = `DeskZen · ${persona.name} · ${cfg?.label ?? ""}`;
  });

  await listen<BubblePayload>("bubble", (e) => {
    showBubble(e.payload.text, e.payload.show_ms);
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
  //（persona-changed / state-changed），否则广播发生在监听器注册之前会被丢弃。
  await invoke("frontend_ready");
}

void init();
