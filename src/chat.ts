import { invoke } from "@tauri-apps/api/core";
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
const stateLabel = document.getElementById("chat-state") as HTMLSpanElement;
const titleEl = document.getElementById("chat-title") as HTMLSpanElement;
const avatarEl = document.getElementById("chat-avatar") as HTMLSpanElement;
const closeBtn = document.getElementById("chat-close") as HTMLButtonElement;
const emptyEl = document.getElementById("chat-empty") as HTMLDivElement;
const sendBtn = document.querySelector("#chat-form .chat-send") as HTMLButtonElement;
const screenBtn = document.getElementById("chat-screen") as HTMLButtonElement;
let persona: PersonaConfig | null = null;
let history: { role: "user" | "assistant"; content: string }[] = [];
let sending = false;
const SCREEN_DEFAULT_QUESTION = "请看看我当前屏幕上的内容，告诉我你看到了什么。";

function stateLabelOf(state: string): string {
  return persona?.states[state]?.label ?? state;
}

function addMessage(role: "user" | "bot", text: string, screen = false): void {
  emptyEl.style.display = "none";
  const div = document.createElement("div");
  div.className = `msg msg-${role}`;
  div.textContent = text;
  if (screen) {
    const tag = document.createElement("span");
    tag.className = "msg-screen-tag";
    tag.textContent = "📷";
    div.appendChild(tag);
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

async function refreshState(): Promise<void> {
  try {
    const state = await invoke<string>("get_current_state");
    stateLabel.textContent = stateLabelOf(state);
  } catch {
    stateLabel.textContent = "…";
  }
}

async function sendMessage(text: string, withScreen: boolean): Promise<void> {
  if (sending) return;
  input.value = "";
  const question = withScreen ? text.trim() || SCREEN_DEFAULT_QUESTION : text;
  addMessage("user", question, withScreen);
  history.push({ role: "user", content: question });
  sending = true;
  sendBtn.disabled = true;
  screenBtn.disabled = true;
  const pending = addTypingIndicator();
  try {
    const { reply, state } = await invoke<ChatReply>("chat_send", {
      messages: history,
      useScreenshot: withScreen,
    });
    stateLabel.textContent = stateLabelOf(state);
    pending.textContent = reply.trim() || "（我好像没看清屏幕，再发一张看看？）";
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
    input.focus();
  }
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  const text = input.value.trim();
  if (!text) return;
  void sendMessage(text, false);
});

screenBtn.addEventListener("click", () => {
  void sendMessage(input.value, true);
});

closeBtn.addEventListener("click", () => {
  void getCurrentWindow().close();
});
// 关闭按钮不在拖拽区域内触发拖动
closeBtn.addEventListener("pointerdown", (e) => e.stopPropagation());

try {
  persona = await invoke<PersonaConfig>("get_persona_config");
} catch {
  persona = null;
}
if (persona) {
  titleEl.textContent = persona.name;
  avatarEl.textContent = persona.name.slice(0, 1);
}
void refreshState();
