import { invoke } from "@tauri-apps/api/core";
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
const sendBtn = document.querySelector("#chat-form button") as HTMLButtonElement;
let persona: PersonaConfig | null = null;
let history: { role: "user" | "assistant"; content: string }[] = [];
let sending = false;

function stateLabelOf(state: string): string {
  return persona?.states[state]?.label ?? state;
}

function addMessage(role: "user" | "bot", text: string): void {
  const div = document.createElement("div");
  div.className = `msg msg-${role}`;
  div.textContent = text;
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

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (sending) return;
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  addMessage("user", text);
  history.push({ role: "user", content: text });
  sending = true;
  sendBtn.disabled = true;
  const pending = addTypingIndicator();
  try {
    const { reply, state } = await invoke<ChatReply>("chat_send", {
      messages: history,
    });
    stateLabel.textContent = stateLabelOf(state);
    pending.textContent = reply;
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
    messages.scrollTop = messages.scrollHeight;
    input.focus();
  }
});

try {
  persona = await invoke<PersonaConfig>("get_persona_config");
} catch {
  persona = null;
}
void refreshState();
