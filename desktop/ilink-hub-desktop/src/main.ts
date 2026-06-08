import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type HubInfo = {
  requestedAddr: string;
  listeningAddr: string | null;
  adminUrl: string | null;
  hubBaseUrl: string | null;
  databasePath: string;
};

type HubClientRow = {
  name: string;
  label: string | null;
  online: boolean;
  vtoken: string;
};

type HubClientsPayload = {
  listening: boolean;
  clients: HubClientRow[];
  authRequired: boolean;
  error: string | null;
};

type HubStatsPayload = {
  listening: boolean;
  error: string | null;
  clientsOnline: number | null;
  clientsTotal: number | null;
  messagesDispatched: number | null;
  upstreamUserMessages: number | null;
};

type RegisterResult = {
  ok: boolean;
  vtoken: string | null;
  baseUrl: string | null;
  authRequired: boolean;
  error: string | null;
};

type QrReady = { kind: "ready"; image: string; link: string };
type QrStatus = { kind: "status"; message: string };
type QrDone = { kind: "done" };
type QrLoginPayload = QrReady | QrStatus | QrDone;

type HubState = "starting" | "running" | "stopped" | "error";

const STATE_LABEL: Record<HubState, string> = {
  starting: "启动中",
  running: "运行中",
  stopped: "已停止",
  error: "出错",
};

let lastQrLink = "";
let hubBaseUrl = "";
let lastRegEnv = "";
let toastTimer: ReturnType<typeof setTimeout> | null = null;
let clientPollTimer: ReturnType<typeof setInterval> | null = null;

function $<T extends HTMLElement>(sel: string): T | null {
  return document.querySelector<T>(sel);
}

function esc(s: string): string {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function initial(name: string): string {
  const t = name.trim();
  return t ? esc(t[0].toUpperCase()) : "?";
}

function envFor(vtoken: string): string {
  return `WEIXIN_BASE_URL=${hubBaseUrl}\nWEIXIN_TOKEN=${vtoken}`;
}

function fmtNum(n: number | null | undefined): string {
  if (n === null || n === undefined) return "—";
  return String(n);
}

function toast(msg: string) {
  const el = $<HTMLElement>("#toast");
  if (!el) return;
  el.textContent = msg;
  el.classList.add("show");
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 2200);
}

async function copyToClipboard(text: string, okMsg = "已复制"): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    toast(okMsg);
    return true;
  } catch {
    window.prompt("请手动复制：", text);
    return false;
  }
}

function setError(msg: string | null) {
  const el = $<HTMLElement>("#error-line");
  if (!el) return;
  if (msg) {
    el.textContent = msg;
    el.hidden = false;
  } else {
    el.hidden = true;
  }
}

const LABEL_ADD_BACKEND = "添加后端";
const LABEL_COLLAPSE_FORM = "收起表单";

/** 收起「添加后端」表单；`clearFeedback` 时清空错误提示与上次注册结果块。 */
function collapseRegisterForm(clearFeedback = false) {
  const wrap = $<HTMLElement>("#register-form-wrap");
  const btn = $<HTMLButtonElement>("#btn-toggle-add");
  if (wrap) wrap.setAttribute("hidden", "");
  if (btn) {
    btn.setAttribute("aria-expanded", "false");
    btn.textContent = LABEL_ADD_BACKEND;
  }
  if (clearFeedback) {
    const msg = $<HTMLElement>("#reg-msg");
    const result = $<HTMLElement>("#reg-result");
    const envEl = $<HTMLElement>("#reg-env");
    if (msg) {
      msg.textContent = "";
      msg.hidden = true;
    }
    if (result) result.hidden = true;
    if (envEl) envEl.textContent = "";
  }
}

function toggleRegisterForm() {
  const wrap = $<HTMLElement>("#register-form-wrap");
  const btn = $<HTMLButtonElement>("#btn-toggle-add");
  if (!wrap || !btn || btn.disabled) return;
  const expanded = !wrap.hidden;
  if (expanded) {
    collapseRegisterForm(false);
  } else {
    wrap.removeAttribute("hidden");
    btn.setAttribute("aria-expanded", "true");
    btn.textContent = LABEL_COLLAPSE_FORM;
    window.requestAnimationFrame(() => {
      $<HTMLInputElement>("#reg-name")?.focus();
    });
  }
}

function setHubState(state: HubState, line?: string) {
  const hero = $<HTMLElement>("#hero");
  const pill = $<HTMLElement>("#status-pill");
  const statusLine = $<HTMLElement>("#status-line");
  if (hero) hero.dataset.state = state;
  if (pill) pill.textContent = STATE_LABEL[state];
  if (line && statusLine) statusLine.textContent = line;

  const btnReg = $<HTMLButtonElement>("#btn-register");
  if (btnReg) btnReg.disabled = state !== "running";

  const btnToggleAdd = $<HTMLButtonElement>("#btn-toggle-add");
  if (btnToggleAdd) {
    btnToggleAdd.disabled = state !== "running";
    if (state !== "running") {
      collapseRegisterForm(true);
    }
  }
}

function showQrModal() {
  $("#qr-modal")?.removeAttribute("hidden");
}

function hideQrModal() {
  $("#qr-modal")?.setAttribute("hidden", "");
  const img = $<HTMLImageElement>("#qr-img");
  if (img) img.removeAttribute("src");
  const st = $("#qr-status");
  if (st) st.textContent = "";
}

function stopClientPolling() {
  if (clientPollTimer !== null) {
    clearInterval(clientPollTimer);
    clientPollTimer = null;
  }
}

function startClientPolling() {
  stopClientPolling();
  void refreshClients();
  void refreshStats();
  clientPollTimer = setInterval(() => {
    void refreshClients();
    void refreshStats();
  }, 10_000);
}

function clearStatsUi() {
  const sec = $<HTMLElement>("#stats-section");
  sec?.setAttribute("hidden", "");
  const ids = ["stat-total", "stat-online", "stat-dispatched", "stat-upstream"];
  for (const id of ids) {
    const el = $(`#${id}`);
    if (el) el.textContent = "—";
  }
}

function applyStatsPayload(s: HubStatsPayload) {
  const sec = $<HTMLElement>("#stats-section");
  if (!s.listening) {
    clearStatsUi();
    return;
  }
  sec?.removeAttribute("hidden");

  const total = s.clientsTotal;
  const online = s.clientsOnline;
  const tEl = $("#stat-total");
  const oEl = $("#stat-online");
  const dEl = $("#stat-dispatched");
  const uEl = $("#stat-upstream");
  if (tEl) tEl.textContent = fmtNum(total);
  if (oEl) {
    if (online !== null && total !== null) {
      oEl.textContent = `${online} / ${total}`;
    } else {
      oEl.textContent = "—";
    }
  }
  if (dEl) dEl.textContent = fmtNum(s.messagesDispatched);
  if (uEl) uEl.textContent = fmtNum(s.upstreamUserMessages);
}

async function refreshStats() {
  try {
    const s = await invoke<HubStatsPayload>("hub_stats");
    applyStatsPayload(s);
  } catch {
    clearStatsUi();
  }
}

function renderClientsEmpty(note: string) {
  const statusEl = $("#clients-status");
  const listEl = $<HTMLUListElement>("#client-list");
  const counter = $<HTMLElement>("#clients-counter");
  if (statusEl) {
    statusEl.textContent = note;
    statusEl.hidden = false;
  }
  if (counter) counter.hidden = true;
  if (listEl) {
    listEl.hidden = true;
    listEl.innerHTML = "";
  }
}

async function refreshClients() {
  const statusEl = $("#clients-status");
  const listEl = $<HTMLUListElement>("#client-list");
  const counter = $<HTMLElement>("#clients-counter");
  if (!statusEl || !listEl || !counter) return;

  try {
    const payload = await invoke<HubClientsPayload>("hub_clients");
    if (!payload.listening) {
      renderClientsEmpty("服务就绪后将自动刷新列表。");
      return;
    }
    if (payload.authRequired) {
      renderClientsEmpty(
        "Hub 已启用 ILINK_ADMIN_TOKEN。请在相同环境变量下启动桌面应用后再注册 / 查看后端。",
      );
      return;
    }
    if (payload.error) {
      renderClientsEmpty(payload.error);
      return;
    }
    const { clients } = payload;
    if (!clients.length) {
      renderClientsEmpty(
        "暂无后端。在列表下方点「添加后端」展开表单，填写名称并注册即可。",
      );
      return;
    }

    const online = clients.filter((c) => c.online).length;
    statusEl.hidden = true;
    counter.hidden = false;
    counter.textContent = `${online} 在线 / 共 ${clients.length}`;
    listEl.hidden = false;
    listEl.innerHTML = clients
      .map(
        (c) => `
      <li class="client-item${c.online ? " is-online" : ""}">
        <span class="client-avatar" aria-hidden="true">${initial(c.name)}</span>
        <span class="client-main">
          <span class="client-name">${esc(c.name)}</span>
          ${c.label ? `<span class="client-label">${esc(c.label)}</span>` : ""}
        </span>
        <span class="client-badge ${c.online ? "online" : "offline"}">${c.online ? "在线" : "离线"}</span>
        <button type="button" class="client-copy" data-vtoken="${esc(c.vtoken)}" title="复制连接配置" aria-label="复制连接配置">
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="9" y="9" width="11" height="11" rx="2" />
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
          </svg>
        </button>
      </li>`,
      )
      .join("");
  } catch (e) {
    renderClientsEmpty(`无法读取后端列表：${String(e)}`);
  }
}

function applyHubInfo(info: HubInfo) {
  const hubUrlEl = $("#hub-base-url");
  const heroHubUrl = $<HTMLElement>("#hero-hub-url");
  const listenNote = $<HTMLElement>("#hero-listen-note");
  const bindHint = $<HTMLElement>("#bind-hint");
  const btnStop = $<HTMLButtonElement>("#btn-stop");

  if (info.listeningAddr) {
    hubBaseUrl = info.hubBaseUrl ?? "";
    const displayUrl = hubBaseUrl || `http://${info.listeningAddr.replace(/^\/*/, "")}`;
    if (hubUrlEl) hubUrlEl.textContent = displayUrl;

    heroHubUrl?.removeAttribute("hidden");

    const listen = info.listeningAddr.trim();
    if (listenNote && listen) {
      if (listen.includes("0.0.0.0") || listen.includes("[::]")) {
        listenNote.textContent = `内核监听：${listen}（后端请使用上方 Hub 地址）`;
        listenNote.removeAttribute("hidden");
      } else {
        listenNote.textContent = "";
        listenNote.setAttribute("hidden", "");
      }
    }

    setHubState("running", "服务已在本机开启。首页可查看统计；后端接入请在「后端」页操作。");
    bindHint?.setAttribute("hidden", "");
    if (btnStop) btnStop.disabled = false;
    startClientPolling();
    void refreshStats();
  } else {
    hubBaseUrl = "";
    if (hubUrlEl) hubUrlEl.textContent = "—";
    heroHubUrl?.setAttribute("hidden", "");
    if (listenNote) {
      listenNote.textContent = "";
      listenNote.setAttribute("hidden", "");
    }
    setHubState(
      "starting",
      "正在启动… 首次使用可能会弹出「微信扫码」；若长时间停留，请看下方提示。",
    );
    bindHint?.removeAttribute("hidden");
    if (btnStop) btnStop.disabled = false;
    stopClientPolling();
    clearStatsUi();
    renderClientsEmpty("服务就绪后将自动刷新列表。");
  }
}

async function refreshHubInfo() {
  try {
    const info = await invoke<HubInfo | null>("hub_info");
    if (!info) {
      setHubState("error", "暂时读不到运行信息");
      clearStatsUi();
      return;
    }
    applyHubInfo(info);
    setError(null);
  } catch (e) {
    setHubState("error", "与程序内部通信失败");
    setError(String(e));
    clearStatsUi();
  }
}

async function registerBackend() {
  const nameEl = $<HTMLInputElement>("#reg-name");
  const labelEl = $<HTMLInputElement>("#reg-label");
  const btn = $<HTMLButtonElement>("#btn-register");
  const msg = $<HTMLElement>("#reg-msg");
  const result = $<HTMLElement>("#reg-result");
  const envEl = $<HTMLElement>("#reg-env");
  if (!nameEl) return;

  const name = nameEl.value.trim();
  const label = labelEl?.value.trim() || null;

  if (msg) msg.hidden = true;
  if (!name) {
    if (msg) {
      msg.textContent = "请先填写后端名称。";
      msg.hidden = false;
    }
    return;
  }

  if (btn) btn.disabled = true;
  try {
    const res = await invoke<RegisterResult>("hub_register", { name, label });
    if (!res.ok) {
      if (msg) {
        msg.textContent = res.error ?? "注册失败，请重试。";
        msg.hidden = false;
      }
      return;
    }
    const env = envFor(res.vtoken ?? "");
    lastRegEnv = env;
    if (envEl) envEl.textContent = env;
    if (result) result.hidden = false;
    nameEl.value = "";
    if (labelEl) labelEl.value = "";
    toast("注册成功");
    collapseRegisterForm(true);
    await refreshClients();
    await refreshStats();
  } catch (e) {
    if (msg) {
      msg.textContent = `注册失败：${String(e)}`;
      msg.hidden = false;
    }
  } finally {
    if (btn) btn.disabled = false;
  }
}

function setActiveTab(which: "home" | "backends") {
  const tabHome = $<HTMLButtonElement>("#tab-home");
  const tabBack = $<HTMLButtonElement>("#tab-backends");
  const panelHome = $<HTMLElement>("#panel-home");
  const panelBack = $<HTMLElement>("#panel-backends");
  if (!tabHome || !tabBack || !panelHome || !panelBack) return;

  const isHome = which === "home";
  tabHome.setAttribute("aria-selected", String(isHome));
  tabBack.setAttribute("aria-selected", String(!isHome));
  panelHome.hidden = !isHome;
  panelBack.hidden = isHome;

  if (!isHome) void refreshClients();
}

window.addEventListener("DOMContentLoaded", () => {
  void refreshHubInfo();

  $("#tab-home")?.addEventListener("click", () => setActiveTab("home"));
  $("#tab-backends")?.addEventListener("click", () => setActiveTab("backends"));

  $("#btn-copy-hub-url")?.addEventListener("click", async (e) => {
    if (!hubBaseUrl) return;
    const btn = e.currentTarget as HTMLButtonElement;
    const ok = await copyToClipboard(hubBaseUrl, "Hub 地址已复制");
    if (ok) {
      btn.classList.add("copied");
      setTimeout(() => btn.classList.remove("copied"), 1200);
    }
  });

  $("#btn-register")?.addEventListener("click", () => void registerBackend());
  $("#btn-toggle-add")?.addEventListener("click", () => toggleRegisterForm());

  $("#reg-name")?.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") void registerBackend();
  });
  $("#reg-label")?.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") void registerBackend();
  });

  $("#btn-copy-env")?.addEventListener("click", () => {
    if (lastRegEnv) void copyToClipboard(lastRegEnv, "连接配置已复制");
  });

  $("#client-list")?.addEventListener("click", async (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(".client-copy");
    if (!btn) return;
    const vtoken = btn.dataset.vtoken;
    if (!vtoken) return;
    const ok = await copyToClipboard(envFor(vtoken), "连接配置已复制");
    if (ok) {
      btn.classList.add("copied");
      setTimeout(() => btn.classList.remove("copied"), 1200);
    }
  });

  $("#btn-stop")?.addEventListener("click", async () => {
    const ok = window.confirm(
      "确定要停止本机 Hub 服务吗？微信中转将暂时不可用，已连接的后端会断开。",
    );
    if (!ok) return;
    try {
      await invoke("stop_hub");
      setHubState("stopped", "已发送停止指令…");
      const btnStop = $<HTMLButtonElement>("#btn-stop");
      if (btnStop) btnStop.disabled = true;
    } catch (err) {
      setError(String(err));
    }
  });

  $("#qr-copy")?.addEventListener("click", () => {
    if (lastQrLink) void copyToClipboard(lastQrLink, "链接已复制，可到微信里粘贴打开");
  });

  void listen<string>("hub-error", (ev) => {
    setError(ev.payload);
    setHubState("error", "启动失败或服务已异常退出");
    hideQrModal();
    stopClientPolling();
    clearStatsUi();
    void refreshHubInfo();
  });

  void listen("hub-stopped", () => {
    setHubState("stopped", "服务已停止");
    const btnStop = $<HTMLButtonElement>("#btn-stop");
    if (btnStop) btnStop.disabled = true;
    $("#hero-hub-url")?.setAttribute("hidden", "");
    hideQrModal();
    stopClientPolling();
    clearStatsUi();
    renderClientsEmpty("服务已停止。");
  });

  void listen<string>("hub-listening", () => {
    void refreshHubInfo();
  });

  void listen<QrLoginPayload>("qr-login", (ev) => {
    const p = ev.payload;
    if (p.kind === "ready") {
      lastQrLink = p.link;
      const img = $<HTMLImageElement>("#qr-img");
      if (img) {
        img.src = p.image;
        img.onerror = () => {
          const st = $("#qr-status");
          if (st) {
            st.textContent =
              "二维码图片加载失败，请点「复制备用链接」到微信中打开。";
          }
        };
      }
      const st = $("#qr-status");
      if (st) st.textContent = "";
      showQrModal();
      setHubState(
        "starting",
        "请先在弹窗里用微信扫码登录；完成后服务会继续启动。",
      );
    } else if (p.kind === "status") {
      const st = $("#qr-status");
      if (st) st.textContent = p.message;
    } else if (p.kind === "done") {
      hideQrModal();
      void refreshHubInfo();
    }
  });
});
