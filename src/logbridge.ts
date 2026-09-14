// 前端日志转发：把 console 的 warn/error 与未捕获错误写进后端的运行日志（与 Rust 侧同一份文件）。
//
// - `console.log` / `console.info` 转发成 debug：默认级别（info）下不会落盘，需要时把级别调到 debug 就能看到；
// - `console.warn` / `console.error`、`window.onerror`、未处理的 Promise 拒绝 → 直接落盘；
// - 转发失败（例如后端还没起来）静默忽略：日志是辅助功能，不能反过来影响界面。
import { invoke } from "@tauri-apps/api/core";

type ForwardLevel = "debug" | "warn" | "error";

const original = {
  log: console.log.bind(console),
  info: console.info.bind(console),
  warn: console.warn.bind(console),
  error: console.error.bind(console),
};

let forwarding = false;

function stringify(value: unknown): string {
  if (typeof value === "string") return value;
  if (value instanceof Error) return value.stack ?? `${value.name}: ${value.message}`;
  if (value === null || value === undefined) return String(value);
  if (typeof value === "object") {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

function forward(level: ForwardLevel, args: unknown[]): void {
  // invoke 自身失败时会再触发 console.error，用重入标记挡住无限递归
  if (forwarding) return;
  forwarding = true;
  try {
    const message = args.map(stringify).join(" ");
    void invoke("log_web", { level, message }).catch(() => {});
  } finally {
    forwarding = false;
  }
}

/** 安装日志桥；每个窗口的入口调用一次 */
export function installLogBridge(): void {
  console.log = (...args: unknown[]) => {
    original.log(...args);
    forward("debug", args);
  };
  console.info = (...args: unknown[]) => {
    original.info(...args);
    forward("debug", args);
  };
  console.warn = (...args: unknown[]) => {
    original.warn(...args);
    forward("warn", args);
  };
  console.error = (...args: unknown[]) => {
    original.error(...args);
    forward("error", args);
  };
  window.addEventListener("error", (event) => {
    forward("error", [`未捕获错误：${event.message}（${event.filename}:${event.lineno}）`]);
  });
  window.addEventListener("unhandledrejection", (event) => {
    forward("error", [`未处理的 Promise 拒绝：${stringify(event.reason)}`]);
  });
}
