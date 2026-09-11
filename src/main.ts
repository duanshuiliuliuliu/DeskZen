import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

interface StateConfig {
  label: string;
  bubbles: string[];
}

interface AnimationClipConfig {
  spritesheet: string;
  frames: number;
  frame_ms: number;
}

interface SceneStepConfig {
  clip: string;
  loops?: number;
}

interface SceneConfig {
  id: string;
  label?: string;
  weight?: number;
  steps: SceneStepConfig[];
}

interface PersonaConfig {
  id: string;
  name: string;
  display_w: number;
  display_h: number;
  states: Record<string, StateConfig>;
  clips?: Record<string, AnimationClipConfig>;
  scenes?: Record<string, SceneConfig[]>;
}

interface StateChangedPayload {
  state: string;
}

interface BubblePayload {
  text: string;
}

let persona: PersonaConfig | null = null;

const character = document.getElementById("character") as HTMLDivElement;
const bubble = document.getElementById("bubble") as HTMLDivElement;
let bubbleTimer: ReturnType<typeof setTimeout> | undefined;
let sceneTimer: ReturnType<typeof setTimeout> | undefined;
let sceneRunId = 0;
let dragging = false;
let pointerStart = { x: 0, y: 0 };

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

/** 角色动作按 id 排序，取第一个可用动作作为兜底/尺寸探测对象 */
function sortedClips(persona: PersonaConfig): [string, AnimationClipConfig][] {
  const clips = persona.clips ?? {};
  return Object.keys(clips)
    .sort()
    .map((id) => [id, clips[id]] as [string, AnimationClipConfig]);
}

/** 应用角色外观：确定角色元素尺寸（具体帧条与动画由 applyClip 设置） */
function applyPersona(persona: PersonaConfig): void {
  if (persona.display_w > 0 && persona.display_h > 0) {
    setCharSize(persona.display_w, persona.display_h);
    applyBubblePosition(persona.display_h);
    return;
  }
  // 未配置显示尺寸：用第一个动作帧条的实际尺寸 ÷ frames 计算
  const clip = sortedClips(persona)[0]?.[1];
  if (!clip) {
    setCharSize(1, 1);
    applyBubblePosition(0);
    return;
  }
  const img = new Image();
  img.onload = () => {
    const frames = Math.max(1, Math.floor(clip.frames));
    const w = Math.round(img.naturalWidth / frames);
    const h = img.naturalHeight;
    setCharSize(w, h);
    applyBubblePosition(h);
  };
  img.onerror = () => {
    setCharSize(1, 1);
    applyBubblePosition(0);
  };
  img.src = resolveSpriteUrl(clip.spritesheet);
}

function chooseScene(scenes: SceneConfig[]): SceneConfig {
  const total = scenes.reduce((sum, scene) => sum + Math.max(1, scene.weight ?? 1), 0);
  let cursor = Math.random() * total;
  for (const scene of scenes) {
    cursor -= Math.max(1, scene.weight ?? 1);
    if (cursor <= 0) return scene;
  }
  return scenes[scenes.length - 1];
}

function applySpriteAnimation(frames: number, frameMs: number): void {
  document.body.style.setProperty("--frames", String(frames));
  // 强制重排后再挂动画，保证换动作时从第 1 帧重新播放（而不是延续上一个动作的进度）
  character.style.animationName = "none";
  void character.offsetWidth;
  character.style.animationDuration = `${frames * frameMs}ms`;
  character.style.animationName = frames > 1 ? "sprite-cycle" : "none";
}

function stopScene(): void {
  sceneRunId += 1;
  if (sceneTimer) clearTimeout(sceneTimer);
  sceneTimer = undefined;
  character.style.animationName = "none";
  delete character.dataset.scene;
  delete character.dataset.clip;
}

function applyClip(clipId: string, clip: AnimationClipConfig): number {
  const frames = Math.max(1, Math.floor(clip.frames));
  const frameMs = Math.max(40, Math.floor(clip.frame_ms));
  document.body.style.setProperty("--cols", String(frames));
  document.body.style.setProperty("--rows", "1");
  character.style.backgroundImage = `url("${resolveSpriteUrl(clip.spritesheet)}")`;
  character.style.backgroundPositionY = "0%";
  character.dataset.clip = clipId;
  applySpriteAnimation(frames, frameMs);
  return frames * frameMs;
}

function startScene(state: string, cfg: StateConfig): void {
  const current = persona;
  const clips = current?.clips ?? {};
  const scenes = (current?.scenes?.[state] ?? []).filter((scene) =>
    scene.steps.some((step) => clips[step.clip]),
  );
  if (scenes.length === 0) {
    // 兜底：手改配置导致状态没有场景时，至少播一个动作，避免角色停留在上一状态的画面
    const fallback = current ? sortedClips(current)[0] : undefined;
    if (fallback) applyClip(fallback[0], fallback[1]);
    return;
  }

  const runId = sceneRunId;
  let previousSceneId = "";

  const runNextScene = (): void => {
    if (runId !== sceneRunId) return;

    let scene = chooseScene(scenes);
    if (scenes.length > 1 && scene.id === previousSceneId) {
      scene = chooseScene(scenes);
    }
    previousSceneId = scene.id;
    character.dataset.scene = scene.id;

    document.title = persona
      ? `DeskZen · ${persona.name} · ${cfg.label}${scene.label ? ` · ${scene.label}` : ""}`
      : "DeskZen";

    const steps = scene.steps.filter((step) => clips[step.clip]);
    let stepIndex = 0;

    const runNextStep = (): void => {
      if (runId !== sceneRunId) return;
      if (stepIndex >= steps.length) {
        runNextScene();
        return;
      }
      const step = steps[stepIndex];
      stepIndex += 1;
      const clip = clips[step.clip];
      const cycleMs = applyClip(step.clip, clip);
      const loops = Math.min(20, Math.max(1, Math.floor(step.loops ?? 1)));
      sceneTimer = setTimeout(runNextStep, cycleMs * loops);
    };

    runNextStep();
  };

  runNextScene();
}

/** 切换角色状态：更新标题后交给场景编排播放该状态的动作片段 */
function applyState(state: string, cfg: StateConfig | undefined): void {
  stopScene();
  document.body.dataset.state = state;
  if (!cfg) return;
  document.title = persona ? `DeskZen · ${persona.name} · ${cfg.label}` : "DeskZen";
  startScene(state, cfg);
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
    stopScene();
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
  //（persona-changed / state-changed），否则广播发生在监听器注册之前会被丢弃。
  await invoke("frontend_ready");
}

void init();
