import { invoke } from "@tauri-apps/api/core";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./styles.css";

interface ChatReply {
  reply: string;
  state: string;
}

interface PersonaConfig {
  name: string;
  states: Record<string, { label: string }>;
}

const messages = document.getElementById("chat-messages") as HTMLDivElement;
const form = document.getElementById("chat-form") as HTMLFormElement;
const input = document.getElementById("chat-input") as HTMLInputElement;
const titleEl = document.getElementById("chat-title") as HTMLSpanElement;
const avatarEl = document.getElementById("chat-avatar") as HTMLSpanElement;
const closeBtn = document.getElementById("chat-close") as HTMLButtonElement;
const sendBtn = document.querySelector("#chat-form .chat-send") as HTMLButtonElement;
const screenBtn = document.getElementById("chat-screen") as HTMLButtonElement;
const chatRoot = document.getElementById("chat-root") as HTMLDivElement;
const attach = document.getElementById("chat-attach") as HTMLDivElement;
const attachImg = document.getElementById("chat-attach-img") as HTMLImageElement;
const attachRemove = document.getElementById("chat-attach-remove") as HTMLButtonElement;
const win = getCurrentWindow();
let persona: PersonaConfig | null = null;
let history: { role: "user" | "assistant"; content: string }[] = [];
let sending = false;
let pendingImage: string | null = null;
let capturing = false;

/** 让气泡高度贴合内容，并重新贴到角色附近 */
async function fitToContent(): Promise<void> {
  const h = Math.min(Math.max(chatRoot.offsetHeight + 12, 150), 480);
  await win.setSize(new LogicalSize(340, h));
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
  tag: "screen" | "image" | null = null,
): void {
  const div = document.createElement("div");
  div.className = `msg msg-${role}`;
  div.textContent = text;
  if (tag) {
    const el = document.createElement("span");
    el.className = "msg-tag";
    el.textContent = tag === "screen" ? "📷" : "🖼";
    div.appendChild(el);
  }
  messages.appendChild(div);
  messages.scrollTop = messages.scrollHeight;
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
  const trimmed = text.trim();
  let question: string;
  if (image) {
    question = trimmed || "请看看这张图片，告诉我你看到了什么。";
  } else {
    if (!trimmed) return;
    question = trimmed;
  }
  input.value = "";
  addMessage("user", question, image ? "image" : null);
  history.push({ role: "user", content: question });
  // 点发送即清除预览（图片已随本次消息一起提交，不影响 image 变量）
  pendingImage = null;
  renderAttach();
  sending = true;
  sendBtn.disabled = true;
  screenBtn.disabled = true;
  const pending = addTypingIndicator();
  void fitToContent();
  try {
    const { reply } = await invoke<ChatReply>("chat_send", {
      messages: history,
      clipboardImage: image,
    });
    const fallback = image ? "我没看清，再发一次看看" : "我没听清，再说一次";
    pending.textContent = reply.trim() || fallback;
    pending.classList.remove("typing");
    history.push({ role: "assistant", content: reply });
    // 只保留最近 20 条，避免上下文无限膨胀
    if (history.length > 20) {
      history = history.slice(history.length - 20);
    }
  } catch (err) {
    pending.textContent = `出错了：${String(err)}`;
    pending.classList.remove("typing");
    history.pop(); // 撤回本次用户消息，允许重试
  } finally {
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
  avatarEl.textContent = persona.name.slice(0, 1);
}
void fitToContent();
