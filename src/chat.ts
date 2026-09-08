import { invoke } from "@tauri-apps/api/core";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

interface ChatReply {
  reply: string;
}

interface PersonaConfig {
  id: string;
  name: string;
  states: Record<string, { label: string }>;
}

type HistoryMessage = { role: "user" | "assistant"; content: string };

const messages = document.getElementById("chat-messages") as HTMLDivElement;
const form = document.getElementById("chat-form") as HTMLFormElement;
const input = document.getElementById("chat-input") as HTMLInputElement;
const titleEl = document.getElementById("chat-title") as HTMLSpanElement;
const closeBtn = document.getElementById("chat-close") as HTMLButtonElement;
const sendBtn = document.querySelector("#chat-form .chat-send") as HTMLButtonElement;
const screenBtn = document.getElementById("chat-screen") as HTMLButtonElement;
const chatRoot = document.getElementById("chat-root") as HTMLDivElement;
const attach = document.getElementById("chat-attach") as HTMLDivElement;
const attachImg = document.getElementById("chat-attach-img") as HTMLImageElement;
const attachRemove = document.getElementById("chat-attach-remove") as HTMLButtonElement;
const win = getCurrentWindow();
let persona: PersonaConfig | null = null;
let history: HistoryMessage[] = [];
let sending = false;
let pendingImage: string | null = null;
let capturing = false;
// 会话代数：切换角色时 +1；进行中的旧请求结果若已过期则丢弃
let chatEpoch = 0;

/** 让气泡高度贴合内容，并重新贴到角色附近 */
async function fitToContent(): Promise<void> {
  // 宽度跟随窗口（lib.rs 定义一次），高度按内容自适应
  const scale = await win.scaleFactor();
  const w = (await win.innerSize()).toLogical(scale).width;
  const h = Math.min(Math.max(chatRoot.offsetHeight + 12, 150), 480);
  await win.setSize(new LogicalSize(w, h));
  await invoke("reposition_chat");
}

/** 显示/隐藏粘贴图片预览 */
function renderAttach(): void {
  if (pendingImage) {
    attachImg.src = pendingImage;
    attach.classList.remove("hidden");
  } else {
    attach.classList.add("hidden");
    attachImg.removeAttribute("src");
  }
  void fitToContent();
}

function addMessage(
  role: "user" | "bot",
  text: string,
  withImage = false,
): void {
  const div = document.createElement("div");
  div.className = `msg msg-${role}`;
  div.textContent = text;
  if (withImage) {
    const el = document.createElement("span");
    el.className = "msg-tag";
    el.textContent = "🖼";
    div.appendChild(el);
  }
  messages.appendChild(div);
  messages.scrollTop = messages.scrollHeight;
}

/** 把内存里的 history 逐条渲染到聊天区（user → 右侧，assistant → 左侧） */
function renderHistoryMessages(): void {
  for (const m of history) {
    addMessage(m.role === "user" ? "user" : "bot", m.content);
  }
}

/** 把当前 history 持久化到该角色名下；发送期间切过角色则丢弃（避免写串别人的历史） */
function persistHistory(pid: string | undefined): void {
  if (!pid) return;
  if (persona?.id !== pid) return; // 会话已切到别的角色，丢弃这次保存
  void invoke("save_chat_history", { personaId: pid, messages: history }).catch((err) => {
    console.error("保存对话历史失败", err);
  });
}

function addTypingIndicator(): HTMLDivElement {
  const div = document.createElement("div");
  div.className = "msg msg-bot typing";
  div.textContent = "正在输入…";
  messages.appendChild(div);
  messages.scrollTop = messages.scrollHeight;
  return div;
}

function loadImage(src: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const im = new Image();
    im.onload = () => resolve(im);
    im.onerror = () => reject(new Error("图片解码失败"));
    im.src = src;
  });
}

/** 压缩/缩小任意图片源（data URL / object URL），避免超大 base64 拖慢 IPC 与大模型 */
async function downscaleToJpeg(src: string): Promise<string> {
  const img = await loadImage(src);
  const maxDim = 1280;
  let { naturalWidth: w, naturalHeight: h } = img;
  if (Math.max(w, h) > maxDim) {
    const scale = maxDim / Math.max(w, h);
    w = Math.round(w * scale);
    h = Math.round(h * scale);
  }
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("Canvas 不可用");
  ctx.drawImage(img, 0, 0, w, h);
  return canvas.toDataURL("image/jpeg", 0.85);
}

async function readClipboardImage(file: File): Promise<string> {
  const url = URL.createObjectURL(file);
  try {
    return await downscaleToJpeg(url);
  } finally {
    URL.revokeObjectURL(url);
  }
}

async function sendMessage(text: string, image: string | null = null): Promise<void> {
  if (sending) return;
  // 用发送时的角色 id 持久化历史；期间切换了角色则由 persistHistory 跳过，避免写串。
  const pid = persona?.id;
  const trimmed = text.trim();
  let question: string;
  if (image) {
    question = trimmed || "请看看这张图片，告诉我你看到了什么。";
  } else {
    if (!trimmed) return;
    question = trimmed;
  }
  input.value = "";
  addMessage("user", question, !!image);
  history.push({ role: "user", content: question });
  // 点发送即清除预览（图片已随本次消息一起提交，不影响 image 变量）
  pendingImage = null;
  renderAttach();
  sending = true;
  sendBtn.disabled = true;
  screenBtn.disabled = true;
  const pending = addTypingIndicator();
  const epoch = chatEpoch;
  let streamStarted = false;
  // 订阅流式增量：首个增量到来时撤下“正在输入…”，随后逐字追加。
  // 回调必须校验 epoch，角色切换后旧会话的增量（连同旧请求）一律丢弃。
  let unlistenDelta: (() => void) | null = null;
  let unlistenReset: (() => void) | null = null;
  void fitToContent();
  try {
    unlistenDelta = await listen<string>("chat-delta", (e) => {
      if (epoch !== chatEpoch) return; // 过期会话的增量丢弃
      const delta = e.payload;
      if (!streamStarted) {
        pending.textContent = "";
        pending.classList.remove("typing");
        streamStarted = true;
      }
      pending.textContent += delta;
      messages.scrollTop = messages.scrollHeight;
    });
    // 空回复重试前由后端发出：把输入区重置为“正在输入…”，避免残留旧流。
    unlistenReset = await listen("chat-reset", () => {
      if (epoch !== chatEpoch) return;
      pending.textContent = "正在输入…";
      pending.classList.add("typing");
      streamStarted = false;
    });
    const { reply } = await invoke<ChatReply>("chat_send", {
      messages: history,
      clipboardImage: image,
    });
    if (epoch !== chatEpoch) return; // 期间切换了角色，丢弃旧会话的回复
    const fallback = image ? "我没看清，再发一次看看" : "我没听清，再说一次";
    pending.textContent = reply.trim() || fallback;
    pending.classList.remove("typing");
    history.push({ role: "assistant", content: reply });
    // 只保留最近 20 条，避免上下文无限膨胀
    if (history.length > 20) {
      history = history.slice(history.length - 20);
    }
    persistHistory(pid);
  } catch (err) {
    if (epoch !== chatEpoch) return; // 期间切换了角色，丢弃旧会话的错误
    if (streamStarted) {
      // 已收到部分回复：把界面上的半截回复作为事实保留（末尾追加错误标注），
      // history 保留用户消息并把半截回复记为 assistant，保证界面与上下文一致；
      // 若此时 pop 用户消息，下一轮上下文会与界面对不上。
      const partial = pending.textContent;
      pending.textContent = `${partial}（回复中断：出错了）`;
      pending.classList.remove("typing");
      history.push({ role: "assistant", content: partial });
      if (history.length > 20) {
        history = history.slice(history.length - 20);
      }
      persistHistory(pid);
    } else {
      // 一个字都没收到：维持现状——显示错误并撤回本次用户消息，允许重试。
      pending.textContent = `出错了：${String(err)}`;
      pending.classList.remove("typing");
      history.pop();
      persistHistory(pid);
    }
  } finally {
    unlistenDelta?.();
    unlistenReset?.();
    sending = false;
    sendBtn.disabled = false;
    screenBtn.disabled = false;
    messages.scrollTop = messages.scrollHeight;
    void fitToContent();
    input.focus();
  }
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  const text = input.value.trim();
  if (!text && !pendingImage) return;
  void sendMessage(text, pendingImage);
});

// 点击「📷」：截取屏幕 → 压缩 → 作为预览，等用户点发送再发
screenBtn.addEventListener("click", async () => {
  if (capturing) return;
  capturing = true;
  screenBtn.disabled = true;
  try {
    const raw = await invoke<string>("capture_screen");
    pendingImage = await downscaleToJpeg(raw);
    renderAttach();
    input.focus();
  } catch (err) {
    console.error("截图失败", err);
  } finally {
    capturing = false;
    screenBtn.disabled = false;
  }
});

// 粘贴图片 → 读成 base64 预览，发送时随消息提交
input.addEventListener("paste", (e) => {
  const items = e.clipboardData?.items;
  if (!items) return;
  for (const item of items) {
    if (item.type.startsWith("image/")) {
      const file = item.getAsFile();
      if (file) {
        e.preventDefault();
        void readClipboardImage(file).then((data) => {
          pendingImage = data;
          renderAttach();
        });
      }
      break;
    }
  }
});

attachRemove.addEventListener("click", () => {
  pendingImage = null;
  renderAttach();
  input.focus();
});

closeBtn.addEventListener("click", () => {
  // 关闭改为隐藏，保留本会话内的对话历史；再次打开时窗口/JS 状态仍在
  void getCurrentWindow().hide();
});

try {
  persona = await invoke<PersonaConfig>("get_persona_config");
} catch {
  persona = null;
}
if (persona) {
  titleEl.textContent = persona.name;
  input.placeholder = `和${persona.name}说点什么…`;
  // 加载该角色各自持久化的对话历史并逐条渲染（启动首开不弹“已切换到”提示）
  const h = await invoke<HistoryMessage[]>("load_chat_history", {
    personaId: persona.id,
  });
  history = h;
  renderHistoryMessages();
}
// 角色窗口可能切换了角色（对话窗是隐藏而非销毁），需同步标题与占位；
// 同时清空旧角色的对话上下文，避免历史混入新角色的 system prompt
void listen<PersonaConfig>("persona-changed", async (e) => {
  persona = e.payload;
  titleEl.textContent = persona.name;
  input.placeholder = `和${persona.name}说点什么…`;
  history = [];
  pendingImage = null;
  chatEpoch += 1;
  renderAttach();
  messages.textContent = "";
  const tip = document.createElement("div");
  tip.className = "msg msg-system";
  tip.textContent = `已切换到「${persona.name}」，开始新对话`;
  messages.appendChild(tip);
  // 加载新角色各自持久化的历史；若加载期间又切了角色，丢弃这次过期结果
  const h = await invoke<HistoryMessage[]>("load_chat_history", {
    personaId: e.payload.id,
  });
  if (persona?.id !== e.payload.id) return;
  history = h;
  renderHistoryMessages();
  void fitToContent();
});
void fitToContent();
