// hrn Browser Relay (MV3 service worker)
// Bridges native messaging host `hrn.relay` to chrome.tabs and chrome.debugger,
// with MV3 alarm + heartbeat keepalive, >1 MiB chunk reassembly, and foreign-extension iframe bypass.

const HOST_NAME = "hrn.relay";
const KEEPALIVE_ALARM = "hrn-relay-keepalive";
const HEARTBEAT_MS = 20000;
const FOREIGN_FRAME_RE = /chrome-extension:\/\/ URL of different extension/i;
const FOREIGN_RETRY_MS = [50, 100, 200, 400];

const attachedTabs = new Set();
const pendingChunks = new Map();
let port = null;
let reconnectTimer = null;
let heartbeatTimer = null;

function startHeartbeat() {
  if (heartbeatTimer) return;
  heartbeatTimer = setInterval(() => {
    if (!port) return;
    try {
      port.postMessage({ ping: true });
    } catch (_) {
      stopHeartbeat();
    }
  }, HEARTBEAT_MS);
}

function stopHeartbeat() {
  if (!heartbeatTimer) return;
  clearInterval(heartbeatTimer);
  heartbeatTimer = null;
}

function connect() {
  if (port) return;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
  } catch (_) {
    scheduleReconnect();
    return;
  }

  startHeartbeat();

  port.onMessage.addListener((msg) => {
    if (!msg || typeof msg !== "object") return;
    if (msg.pong === true) return;
    if (msg.cmd === "ping") {
      try {
        port.postMessage({ pong: true });
      } catch (_) {}
      return;
    }
    if (msg.cmd === "chunk") {
      handleChunk(msg);
      return;
    }
    dispatchMessage(msg);
  });

  port.onDisconnect.addListener(() => {
    port = null;
    stopHeartbeat();
    pendingChunks.clear();
    scheduleReconnect();
  });
}

function scheduleReconnect() {
  if (reconnectTimer) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, 2000);
}

function handleChunk(msg) {
  const parts = pendingChunks.get(msg.chunkId) || [];
  parts[msg.index] = msg.data || "";
  if (!msg.last) {
    pendingChunks.set(msg.chunkId, parts);
    return;
  }
  pendingChunks.delete(msg.chunkId);
  try {
    const parsed = JSON.parse(parts.join(""));
    dispatchMessage(parsed);
  } catch (err) {
    console.error("hrn relay chunk decode failed:", err);
  }
}

function dispatchMessage(msg) {
  handleMessage(msg).catch((err) => {
    if (msg && msg.id !== undefined) {
      postReply(msg.id, false, null, String(err && err.message ? err.message : err));
    }
  });
}

function postReply(id, ok, result, error) {
  if (!port) return;
  const msg = ok ? { id, ok: true, result: result ?? {} } : { id, ok: false, error: error || "relay error" };
  try {
    port.postMessage(msg);
  } catch (_) {}
}

async function neutralizeForeignFrames(tabId) {
  if (!chrome.scripting) return 0;
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      injectImmediately: true,
      args: [chrome.runtime.id],
      func: (selfId) => {
        let swept = 0;
        const visit = (root) => {
          const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
          for (let el = walker.nextNode(); el; el = walker.nextNode()) {
            const tag = el.tagName ? el.tagName.toUpperCase() : "";
            if (tag === "IFRAME" || tag === "FRAME") {
              const src = el.getAttribute("src") || "";
              if (src.startsWith("chrome-extension://") && !src.startsWith(`chrome-extension://${selfId}/`)) {
                if (tag === "FRAME") {
                  el.setAttribute("src", "about:blank");
                } else if (el.getAttribute("srcdoc") === null) {
                  el.setAttribute("srcdoc", "");
                }
                el.style.setProperty("display", "none", "important");
                swept++;
              }
            }
            try {
              const shadow = chrome.dom && chrome.dom.openOrClosedShadowRoot
                ? chrome.dom.openOrClosedShadowRoot(el)
                : el.shadowRoot;
              if (shadow) visit(shadow);
            } catch (_) {}
          }
        };
        visit(document);
        return swept;
      },
    });
    return results.reduce((sum, r) => sum + (r && typeof r.result === "number" ? r.result : 0), 0);
  } catch (_) {
    return 0;
  }
}

async function pastForeignFrames(tabId, fn) {
  for (let attempt = 0; ; attempt++) {
    try {
      return await fn();
    } catch (err) {
      if (
        attempt >= FOREIGN_RETRY_MS.length ||
        !FOREIGN_FRAME_RE.test(String(err)) ||
        (await neutralizeForeignFrames(tabId)) === 0
      ) {
        throw err;
      }
      await new Promise((resolve) => setTimeout(resolve, FOREIGN_RETRY_MS[attempt]));
    }
  }
}

async function ensureAttached(tabId) {
  if (attachedTabs.has(tabId)) return;
  await pastForeignFrames(tabId, () => chrome.debugger.attach({ tabId }, "1.3"));
  attachedTabs.add(tabId);
}

async function handleMessage(msg) {
  if (!msg || typeof msg !== "object") return;
  const { id, cmd } = msg;
  if (cmd === "open_tab") {
    const url = msg.url || "about:blank";
    // Always open in background without stealing active tab focus.
    const tab = await chrome.tabs.create({ url, active: false });
    await ensureAttached(tab.id);
    postReply(id, true, { tabId: tab.id, url: tab.url || url, title: tab.title || "" });
    return;
  }
  if (cmd === "list_tabs") {
    const tabs = await chrome.tabs.query({});
    postReply(id, true, {
      tabs: tabs.map((t) => ({
        tabId: t.id,
        url: t.url || "",
        title: t.title || "",
        active: Boolean(t.active),
      })),
    });
    return;
  }
  if (cmd === "attach_tab") {
    const tabId = Number(msg.tabId);
    await ensureAttached(tabId);
    postReply(id, true, { tabId });
    return;
  }
  if (cmd === "detach_tab") {
    const tabId = Number(msg.tabId);
    if (attachedTabs.has(tabId)) {
      attachedTabs.delete(tabId);
      try {
        await chrome.debugger.detach({ tabId });
      } catch (_) {}
    }
    postReply(id, true, {});
    return;
  }
  if (cmd === "close_tab") {
    const tabId = Number(msg.tabId);
    if (attachedTabs.has(tabId)) {
      attachedTabs.delete(tabId);
      try {
        await chrome.debugger.detach({ tabId });
      } catch (_) {}
    }
    await chrome.tabs.remove(tabId);
    postReply(id, true, {});
    return;
  }
  if (cmd === "cdp") {
    const tabId = Number(msg.tabId);
    await ensureAttached(tabId);
    const result = await pastForeignFrames(tabId, () =>
      chrome.debugger.sendCommand({ tabId }, msg.method, msg.params || {})
    );
    postReply(id, true, result || {});
    return;
  }
  postReply(id, false, null, `unknown relay command: ${cmd}`);
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (!port || source.tabId === undefined) return;
  try {
    port.postMessage({ event: true, tabId: source.tabId, method, params: params || {} });
  } catch (_) {}
});

chrome.debugger.onDetach.addListener((source) => {
  if (source.tabId !== undefined) {
    attachedTabs.delete(source.tabId);
  }
});

chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === KEEPALIVE_ALARM) {
    connect();
    if (port) {
      try {
        port.postMessage({ ping: true });
      } catch (_) {}
    }
  }
});

chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
