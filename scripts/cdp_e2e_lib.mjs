const CDP_PORT = 9222;

/** 角色主页目标过滤：生产包为 http://tauri.localhost/，dev（vite）为 http://localhost:1420/ */
export const personaTargetFilter = (t) =>
  t.type === "page" &&
  /^http:\/\/(tauri\.localhost|localhost:1420)$/.test(t.url.replace(/\/$/, ""));

export async function getTargets(filter) {
  for (let i = 0; i < 40; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${CDP_PORT}/json`);
      const targets = await res.json();
      const hit = targets.find(filter);
      if (hit) return hit;
    } catch {
      // app 尚未就绪
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("找不到匹配的页面目标（请确认以 CDP 端口启动）");
}

export function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let seq = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    }
  };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = () => reject(new Error("WebSocket 连接失败"));
  });
  return {
    ready,
    send(method, params = {}) {
      const id = ++seq;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ id, method, params }));
      });
    },
    close() {
      ws.close();
    },
  };
}

export async function evaluate(client, expression, awaitPromise = true) {
  const res = await client.send("Runtime.evaluate", {
    expression,
    awaitPromise,
    returnByValue: true,
  });
  if (res.exceptionDetails) {
    throw new Error(
      res.exceptionDetails.exception?.description ?? res.exceptionDetails.text,
    );
  }
  return res.result.value;
}
