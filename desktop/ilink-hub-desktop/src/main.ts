import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type HubInfo = {
  requestedAddr: string;
  listeningAddr: string | null;
  adminUrl: string | null;
  hubBaseUrl: string | null;
  databasePath: string;
};

type DesktopSettingsPayload = {
  listenPort: number;
  requestedAddr: string;
};

type SetListenPortResult = {
  ok: boolean;
  requestedAddr: string;
  listenPort: number;
  error: string | null;
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

type DeleteClientResult = {
  ok: boolean;
  authRequired: boolean;
  error: string | null;
};

type UpdateClientResult = {
  ok: boolean;
  name: string | null;
  authRequired: boolean;
  error: string | null;
};

type EditingClient = {
  oldName: string;
  name: string;
  label: string;
};

type BridgeStatusPayload = {
  configured: boolean;
  path: string;
  state: "starting" | "running" | "stopped" | "error" | string;
  running: boolean;
  error: string | null;
  manager: BridgeManagerStatus | null;
};

type BridgeManagerStatus = {
  state: string;
  profilesTotal: number;
  running: number;
  restarting: number;
  children: BridgeChildStatus[];
  lastError: string | null;
};

type BridgeChildStatus = {
  id: string;
  configPath: string;
  registerName: string;
  state: string;
  pid: number | null;
  uptimeSecs: number | null;
  restartAttempts: number;
  nextRestartSecs: number | null;
  lastError: string | null;
};

type EnvVar = { key: string; value: string };

type BridgeProfileFile = {
  id: string;
  path: string;
  valid: boolean;
  error: string | null;
  template: string;
  yaml: string;
  profiles: string[];
  defaultProfile: string | null;
  routing: string | null;
  cwd: string | null;
  timeoutSecs: number;
  maxReplyChars: number;
  model: string | null;
  command: string | null;
  args: string[];
  envVars: EnvVar[];
};

type BridgeProfilesPayload = {
  profilesDir: string;
  credentialsDir: string;
  profiles: BridgeProfileFile[];
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
let bridgeConfigPath = "";
let bridgeProfilesDir = "";
let bridgeCredentialsDir = "";
let bridgeProfiles: BridgeProfileFile[] = [];
let selectedBridgeProfileId: string | null = null;
let bridgeAutoStartAttempted = false;
/** Set to true by applyBridgeStatus whenever bridge is running/starting.
 * Captured in the hub-stopped handler so that when hub comes back on a
 * new port we know whether to restart the bridge manager (bridge_restart)
 * rather than just doing the first-run auto-start check. */
let lastBridgeRunning = false;
let bridgeNeedsRestartOnHubRecovery = false;
let editingClient: EditingClient | null = null;
let confirmResolver: ((ok: boolean) => void) | null = null;
let toastTimer: ReturnType<typeof setTimeout> | null = null;
let clientPollTimer: ReturnType<typeof setInterval> | null = null;
let bridgePollTimer: ReturnType<typeof setInterval> | null = null;

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

function closeConfirmModal(ok: boolean) {
  $("#confirm-modal")?.setAttribute("hidden", "");
  if (confirmResolver) {
    confirmResolver(ok);
    confirmResolver = null;
  }
}

function openConfirmModal(message: string, title = "请确认"): Promise<boolean> {
  return new Promise((resolve) => {
    const modal = $<HTMLElement>("#confirm-modal");
    const msg = $<HTMLElement>("#confirm-message");
    const titleEl = $<HTMLElement>("#confirm-title");
    if (!modal || !msg) {
      resolve(false);
      return;
    }
    if (titleEl) titleEl.textContent = title;
    msg.textContent = message;
    confirmResolver = resolve;
    modal.removeAttribute("hidden");
  });
}

type PortConflictResolver = ((port: number | null) => void) | null;
let portConflictResolver: PortConflictResolver = null;

function closePortConflictModal(port: number | null) {
  $("#port-conflict-modal")?.setAttribute("hidden", "");
  const msg = $<HTMLElement>("#port-conflict-msg");
  if (msg) {
    msg.textContent = "";
    msg.hidden = true;
  }
  const input = $<HTMLInputElement>("#port-conflict-input");
  if (input) input.value = "";
  if (portConflictResolver) {
    portConflictResolver(port);
    portConflictResolver = null;
  }
}

function openPortConflictModal(
  message: string,
  suggestedPort: number,
): Promise<number | null> {
  return new Promise((resolve) => {
    const modal = $<HTMLElement>("#port-conflict-modal");
    const msg = $<HTMLElement>("#port-conflict-message");
    const input = $<HTMLInputElement>("#port-conflict-input");
    if (!modal || !msg || !input) {
      resolve(null);
      return;
    }
    msg.textContent = message;
    input.value = String(suggestedPort);
    portConflictResolver = resolve;
    modal.removeAttribute("hidden");
    requestAnimationFrame(() => {
      input.focus();
      input.select();
    });
  });
}

const actionIcon = {
  edit: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d="M12 20h9" />
    <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
  </svg>`,
  copy: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <rect x="9" y="9" width="11" height="11" rx="2" />
    <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
  </svg>`,
  delete: `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d="M3 6h18" />
    <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
    <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
    <path d="M10 11v6" />
    <path d="M14 11v6" />
  </svg>`,
} as const;

function renderClientItem(c: HubClientRow): string {
  const isEditing = editingClient?.oldName === c.name;
  const editName = isEditing ? editingClient!.name : c.name;
  const editLabel = isEditing ? editingClient!.label : (c.label ?? "");
  return `
      <li class="client-item${c.online ? " is-online" : ""}${isEditing ? " is-editing" : ""}" data-client-name="${esc(c.name)}">
        <div class="client-row client-view">
          <span class="client-avatar" aria-hidden="true">${initial(c.name)}</span>
          <span class="client-main">
            <span class="client-name">${esc(c.name)}</span>
            ${c.label ? `<span class="client-label">${esc(c.label)}</span>` : ""}
          </span>
          <span class="client-badge ${c.online ? "online" : "offline"}">${c.online ? "在线" : "离线"}</span>
          <div class="client-actions">
            <button type="button" class="client-action client-edit-btn" data-action="edit" title="编辑名称与备注" aria-label="编辑 ${esc(c.name)}">
              ${actionIcon.edit}
            </button>
            <button type="button" class="client-action client-copy" data-action="copy" data-vtoken="${esc(c.vtoken)}" title="复制连接配置" aria-label="复制连接配置">
              ${actionIcon.copy}
            </button>
            ${
              c.online
                ? ""
                : `<button type="button" class="client-action client-delete" data-action="delete" title="删除离线后端" aria-label="删除离线后端 ${esc(c.name)}">
              ${actionIcon.delete}
            </button>`
            }
          </div>
        </div>
        <div class="client-edit">
          <div class="client-edit-fields">
            <label class="field client-edit-field">
              <span class="field-label">名称</span>
              <input type="text" data-field="name" value="${esc(editName)}" autocomplete="off" spellcheck="false" />
            </label>
            <label class="field client-edit-field">
              <span class="field-label">备注</span>
              <input type="text" data-field="label" value="${esc(editLabel)}" placeholder="可选" autocomplete="off" />
            </label>
          </div>
          <div class="client-edit-actions">
            <button type="button" class="btn btn-primary btn-sm" data-action="save">保存</button>
            <button type="button" class="btn btn-ghost btn-sm" data-action="cancel">取消</button>
          </div>
        </div>
      </li>`;
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

function setBridgeMessage(msg: string | null) {
  const el = $<HTMLElement>("#bridge-msg");
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

// ─── Listen-port setting (M2 — UX-02) ───────────────────────────────────────

let lastListenPort: number | null = null;

function parsePortFromAddr(addr: string | null | undefined): number | null {
  if (!addr) return null;
  const trimmed = addr.trim();
  if (!trimmed) return null;
  // Accept 127.0.0.1:<port>, localhost:<port>, or a bare <port>.
  const parts = trimmed.split(":");
  if (parts.length === 1) {
    const n = Number(parts[0]);
    return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
  }
  const portStr = parts[parts.length - 1];
  const n = Number(portStr);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
}

function setPortMsg(msg: string | null, isError = true) {
  const el = $<HTMLElement>("#port-msg");
  if (!el) return;
  if (!msg) {
    el.textContent = "";
    el.hidden = true;
    return;
  }
  el.textContent = msg;
  el.hidden = false;
  el.style.color = isError ? "" : "var(--ok)";
}

function prefillListenPort(port: number | null) {
  const input = $<HTMLInputElement>("#listen-port-input");
  if (!input) return;
  if (port !== null) {
    input.value = String(port);
    input.placeholder = String(port);
    lastListenPort = port;
  }
}

async function refreshDesktopSettings() {
  try {
    const s = await invoke<DesktopSettingsPayload>("get_desktop_settings");
    prefillListenPort(s.listenPort);
    return s;
  } catch {
    // Keep last known port; UI will fall back to whatever the input shows.
    return null;
  }
}

/** Heuristic: decide whether a hub-error payload points at a bind conflict. */
function isPortBindError(message: string | null | undefined): boolean {
  if (!message) return false;
  const m = message.toLowerCase();
  if (m.includes("address already in use")) return true;
  if (m.includes("already in use")) return true;
  if (m.includes("bind")) return true;
  if (m.includes("eaddrinuse")) return true;
  return false;
}

function extractPortFromError(message: string): number | null {
  // Try to read back the offending port from the error text (e.g.
  // "address already in use (os error 48) at 127.0.0.1:8765"). Falls back
  // to the active requested_addr's port when nothing specific is parseable.
  const m = String(message);
  const re = /(?:127\.0\.0\.1|localhost|0\.0\.0\.0|\[::\]):(\d{1,5})/i;
  const hit = m.match(re);
  if (hit) {
    const n = Number(hit[1]);
    if (Number.isInteger(n) && n >= 1 && n <= 65535) return n;
  }
  return lastListenPort;
}

async function applyListenPortChange(newPort: number): Promise<boolean> {
  if (!Number.isInteger(newPort) || newPort < 1 || newPort > 65535) {
    setPortMsg("端口必须在 1..=65535 之间");
    return false;
  }
  const btn = $<HTMLButtonElement>("#btn-save-port");
  if (btn) btn.disabled = true;
  try {
    const res = await invoke<SetListenPortResult>("set_listen_port", { port: newPort });
    if (!res.ok) {
      setPortMsg(res.error ?? "保存端口失败");
      return false;
    }
    prefillListenPort(res.listenPort);
    setPortMsg(`已保存为 ${res.listenPort}，正在重启 Hub…`, false);

    // Restart the hub so the new port actually takes effect.
    // Always use restart_hub regardless of whether the hub is currently
    // listening: restart_hub handles both cases (slot occupied → stop then
    // start; slot empty → start directly) and avoids the "hub already
    // running" error that start_hub returns when a previous bind failure
    // left a stale sender in the slot.
    try {
      await invoke("restart_hub");
    } catch (err) {
      setPortMsg(`重启失败：${String(err)}`);
      return false;
    }
    setHubState("starting", "正在重新启动 Hub…");
    void refreshHubInfo();
    return true;
  } finally {
    if (btn) btn.disabled = false;
  }
}

async function handleBindFailure(message: string) {
  const activePort = extractPortFromError(message) ?? lastListenPort ?? 8765;
  const suggested = activePort + 1 > 65535 ? Math.max(1, activePort - 1) : activePort + 1;
  // Don't double-open if a modal is already up.
  const existing = $<HTMLElement>("#port-conflict-modal");
  if (existing && !existing.hidden) return;
  const choice = await openPortConflictModal(
    `当前端口 ${activePort} 已被占用，无法启动 Hub。请换一个端口再试。`,
    suggested,
  );
  if (choice === null) return;
  // Apply the chosen port; on failure, the inline form-msg surfaces the reason.
  await applyListenPortChange(choice);
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

  const btnStart = $<HTMLButtonElement>("#btn-start");
  const btnStop = $<HTMLButtonElement>("#btn-stop");
  // Footer button toggles between stop (danger) and start (primary) based on hub state.
  // Stop is only meaningful when the hub is actually running; start is the recovery
  // affordance for the stopped / starting-failed case so the user never gets stuck.
  const showStart = state === "stopped" || state === "error";
  const showStop = state === "running" || state === "starting";
  if (btnStart) {
    if (showStart) btnStart.removeAttribute("hidden");
    else btnStart.setAttribute("hidden", "");
    btnStart.disabled = state === "starting";
  }
  if (btnStop) {
    if (showStop) btnStop.removeAttribute("hidden");
    else btnStop.setAttribute("hidden", "");
    btnStop.disabled = state !== "running";
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

function bridgeStateLabel(state: BridgeStatusPayload["state"]): string {
  if (state === "running") return "运行中";
  if (state === "starting") return "启动中";
  if (state === "error") return "出错";
  return "未启动";
}

function applyBridgeStatus(s: BridgeStatusPayload) {
  lastBridgeRunning = s.running || s.state === "starting";
  bridgeConfigPath = s.path || bridgeConfigPath;
  const stateEl = $<HTMLElement>("#bridge-state");
  const pathEl = $<HTMLElement>("#bridge-config-path");
  const startBtn = $<HTMLButtonElement>("#btn-bridge-start");
  const restartBtn = $<HTMLButtonElement>("#btn-bridge-restart");
  const stopBtn = $<HTMLButtonElement>("#btn-bridge-stop");

  if (stateEl) {
    stateEl.textContent = bridgeStateLabel(s.state);
    stateEl.dataset.state = s.state;
  }
  if (pathEl && s.path) pathEl.textContent = s.path;
  if (startBtn) startBtn.disabled = s.running || !hubBaseUrl;
  if (restartBtn) restartBtn.disabled = !hubBaseUrl || !s.configured;
  if (stopBtn) stopBtn.disabled = !s.running;
  const list = $<HTMLElement>("#bridge-profile-list");
  if (list) list.dataset.managerStatus = JSON.stringify(s.manager ?? null);
  if (s.error) setBridgeMessage(s.error);
  renderBridgeProfiles();
}

async function refreshBridgeStatus() {
  try {
    const status = await invoke<BridgeStatusPayload>("bridge_status");
    applyBridgeStatus(status);
  } catch (e) {
    setBridgeMessage(`读取 Bridge 状态失败：${String(e)}`);
  }
}

function startBridgePolling() {
  if (bridgePollTimer !== null) return;
  void refreshBridgeStatus();
  bridgePollTimer = setInterval(() => void refreshBridgeStatus(), 3000);
}

function stopBridgePolling() {
  if (bridgePollTimer !== null) {
    clearInterval(bridgePollTimer);
    bridgePollTimer = null;
  }
}

function templateLabel(template: string): string {
  if (template === "claude") return "Claude Code";
  if (template === "cursor") return "Cursor";
  if (template === "codex") return "Codex";
  if (template === "gemini") return "Gemini";
  return "Custom YAML";
}

function currentManagerChild(id: string): BridgeChildStatus | null {
  // The latest status is reflected through the DOM polling path; this helper is filled by render.
  const statusJson = $<HTMLElement>("#bridge-profile-list")?.dataset.managerStatus;
  if (!statusJson) return null;
  try {
    const status = JSON.parse(statusJson) as BridgeManagerStatus;
    if (!status || !Array.isArray(status.children)) return null;
    return status.children.find((c) => c.id === id) ?? null;
  } catch {
    return null;
  }
}

function renderBridgeProfiles() {
  const list = $<HTMLElement>("#bridge-profile-list");
  const summary = $<HTMLElement>("#bridge-summary");
  if (!list) return;
  if (!bridgeProfiles.length) {
    list.innerHTML = `<div class="bridge-empty">还没有 profile。点击「新建 Profile」开始。</div>`;
    if (summary) summary.textContent = `Profiles 目录：${bridgeProfilesDir || "—"}`;
    return;
  }
  list.innerHTML = bridgeProfiles
    .map((p) => {
      const child = currentManagerChild(p.id);
      const state = !p.valid ? "invalid" : child?.state ?? "stopped";
      const detail = child
        ? `pid ${child.pid ?? "—"} · ${child.uptimeSecs ?? 0}s · 重启 ${child.restartAttempts}`
        : p.valid
          ? "等待 manager 扫描"
          : p.error || "配置无效";
      return `<button type="button" class="bridge-profile-item ${p.id === selectedBridgeProfileId ? "active" : ""}" data-profile-id="${esc(p.id)}">
        <span class="profile-title">${esc(p.id)}</span>
        <span class="profile-meta">${esc(templateLabel(p.template))} · ${esc(state)}</span>
        <span class="profile-detail">${esc(detail)}</span>
      </button>`;
    })
    .join("");
  if (summary) {
    summary.textContent = `${bridgeProfiles.length} 个 profile；目录 ${bridgeProfilesDir || "—"}；凭证 ${bridgeCredentialsDir || "—"}`;
  }
}

function applyBridgeProfiles(payload: BridgeProfilesPayload) {
  bridgeProfilesDir = payload.profilesDir;
  bridgeCredentialsDir = payload.credentialsDir;
  bridgeConfigPath = payload.profilesDir;
  const pathEl = $<HTMLElement>("#bridge-config-path");
  if (pathEl) pathEl.textContent = payload.profilesDir || "—";
  bridgeProfiles = payload.profiles;
  if (payload.error) setBridgeMessage(payload.error);
  if (!selectedBridgeProfileId || !bridgeProfiles.some((p) => p.id === selectedBridgeProfileId)) {
    selectedBridgeProfileId = bridgeProfiles[0]?.id ?? null;
  }
  renderBridgeProfiles();
  const selected = bridgeProfiles.find((p) => p.id === selectedBridgeProfileId);
  if (selected) fillBridgeProfileEditor(selected);
  else newBridgeProfile();
}

async function refreshBridgeConfig() {
  try {
    const profiles = await invoke<BridgeProfilesPayload>("bridge_profiles");
    applyBridgeProfiles(profiles);
    await refreshBridgeStatus();
  } catch (e) {
    setBridgeMessage(`读取 Bridge 配置失败：${String(e)}`);
  }
}

async function startBridge(silent = false) {
  try {
    await invoke("bridge_start");
    if (!silent) toast("Bridge 已启动");
    setBridgeMessage(null);
  } catch (e) {
    if (!silent) setBridgeMessage(String(e));
  } finally {
    await refreshBridgeStatus();
  }
}

async function stopBridge() {
  try {
    await invoke("bridge_stop");
    toast("Bridge 已停止");
    setBridgeMessage(null);
  } catch (e) {
    setBridgeMessage(String(e));
  } finally {
    await refreshBridgeStatus();
  }
}

async function restartBridge() {
  try {
    await invoke("bridge_restart");
    toast("Bridge 已重启");
    setBridgeMessage(null);
  } catch (e) {
    setBridgeMessage(String(e));
  } finally {
    await refreshBridgeStatus();
  }
}

async function autoStartBridgeIfReady() {
  if (bridgeAutoStartAttempted || !hubBaseUrl) return;
  bridgeAutoStartAttempted = true;
  try {
    const payload = await invoke<BridgeProfilesPayload>("bridge_profiles");
    applyBridgeProfiles(payload);
    if (payload.profiles.some((p) => p.valid)) {
      await startBridge(true);
    }
  } catch {
    // The Bridge page shows detailed errors when the user opens it.
  }
}

// ─── Env-var row helpers ──────────────────────────────────────────────────────

function addEnvVarRow(key = "", value = "") {
  const list = $<HTMLElement>("#bridge-env-vars");
  if (!list) return;
  const row = document.createElement("div");
  row.className = "env-var-row";
  row.innerHTML = `
    <input type="text" class="env-key" placeholder="KEY" autocomplete="off" spellcheck="false" />
    <span class="env-equals">=</span>
    <input type="text" class="env-val" placeholder="value" autocomplete="off" spellcheck="false" />
    <button type="button" class="btn btn-ghost btn-xs btn-remove-env" title="删除" aria-label="删除环境变量">×</button>
  `;
  (row.querySelector(".env-key") as HTMLInputElement).value = key;
  (row.querySelector(".env-val") as HTMLInputElement).value = value;
  row.querySelector(".btn-remove-env")?.addEventListener("click", () => row.remove());
  list.appendChild(row);
}

function clearEnvVars() {
  const list = $<HTMLElement>("#bridge-env-vars");
  if (list) list.innerHTML = "";
}

function getEnvVars(): EnvVar[] {
  const rows = document.querySelectorAll<HTMLDivElement>("#bridge-env-vars .env-var-row");
  const result: EnvVar[] = [];
  rows.forEach((row) => {
    const key = (row.querySelector<HTMLInputElement>(".env-key")?.value ?? "").trim();
    const value = (row.querySelector<HTMLInputElement>(".env-val")?.value ?? "").trim();
    if (key) result.push({ key, value });
  });
  return result;
}

// ─── Template visibility ──────────────────────────────────────────────────────

function setTemplateVisibility() {
  const template = $<HTMLSelectElement>("#bridge-template")?.value ?? "claude";
  const isCustom = template === "custom";
  const presetFields = $<HTMLElement>(".bridge-preset-fields");
  const yamlField = $<HTMLElement>(".bridge-yaml-field");
  if (presetFields) presetFields.hidden = isCustom;
  if (yamlField) yamlField.hidden = !isCustom;
}

function fillBridgeProfileEditor(p: BridgeProfileFile) {
  selectedBridgeProfileId = p.id;
  $<HTMLInputElement>("#bridge-profile-id")!.value = p.id;
  $<HTMLSelectElement>("#bridge-template")!.value = p.template;
  $<HTMLInputElement>("#bridge-cwd")!.value = p.cwd ?? "";
  $<HTMLTextAreaElement>("#bridge-yaml")!.value = p.yaml || "";
  clearEnvVars();
  for (const ev of p.envVars ?? []) {
    addEnvVarRow(ev.key, ev.value);
  }
  setTemplateVisibility();
  renderBridgeProfiles();
}

function newBridgeProfile() {
  selectedBridgeProfileId = null;
  $<HTMLInputElement>("#bridge-profile-id")!.value = "";
  $<HTMLSelectElement>("#bridge-template")!.value = "claude";
  $<HTMLInputElement>("#bridge-cwd")!.value = "";
  $<HTMLTextAreaElement>("#bridge-yaml")!.value = "";
  clearEnvVars();
  setTemplateVisibility();
  renderBridgeProfiles();
}

async function saveBridgeProfile() {
  const template = $<HTMLSelectElement>("#bridge-template")?.value ?? "claude";
  const req = {
    originalId: selectedBridgeProfileId,
    id: $<HTMLInputElement>("#bridge-profile-id")?.value.trim() ?? "",
    template,
    cwd: $<HTMLInputElement>("#bridge-cwd")?.value.trim() ?? "",
    envVars: getEnvVars(),
    yaml: $<HTMLTextAreaElement>("#bridge-yaml")?.value ?? "",
  };
  try {
    const saved = await invoke<BridgeProfileFile>("bridge_save_profile", { req });
    selectedBridgeProfileId = saved.id;
    toast("Profile 已保存");
    await refreshBridgeConfig();
  } catch (e) {
    setBridgeMessage(String(e));
  }
}

async function deleteBridgeProfile() {
  if (!selectedBridgeProfileId) return;
  if (!window.confirm(`删除 profile「${selectedBridgeProfileId}」？`)) return;
  try {
    await invoke("bridge_delete_profile", { id: selectedBridgeProfileId });
    selectedBridgeProfileId = null;
    toast("Profile 已删除");
    await refreshBridgeConfig();
  } catch (e) {
    setBridgeMessage(String(e));
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
    listEl.innerHTML = clients.map((c) => renderClientItem(c)).join("");
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

  // Keep the listen-port input in sync with the controller's view of the
  // requested address (which may have been changed by the user since the
  // input was last touched).
  const requestedPort = parsePortFromAddr(info.requestedAddr);
  if (requestedPort !== null) {
    prefillListenPort(requestedPort);
  }

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
    startBridgePolling();
    void refreshStats();
    void refreshBridgeConfig();

    if (bridgeNeedsRestartOnHubRecovery) {
      // Hub came back (possibly on a new port) while bridge was running.
      // Restart the bridge manager so it reconnects to the new hub URL.
      bridgeNeedsRestartOnHubRecovery = false;
      void invoke("bridge_restart").catch(() => {});
    } else {
      void autoStartBridgeIfReady();
    }
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
    stopBridgePolling();
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

function startEditClient(name: string, label: string | null) {
  editingClient = { oldName: name, name, label: label ?? "" };
  void refreshClients();
}

function cancelEditClient() {
  editingClient = null;
  void refreshClients();
}

async function saveEditClient(item: HTMLLIElement) {
  const oldName = item.dataset.clientName;
  if (!oldName) return;
  const name = item.querySelector<HTMLInputElement>('[data-field="name"]')?.value.trim() ?? "";
  const label = item.querySelector<HTMLInputElement>('[data-field="label"]')?.value.trim() ?? "";
  if (!name) {
    toast("请填写后端名称");
    return;
  }
  try {
    const res = await invoke<UpdateClientResult>("hub_update_client", {
      oldName,
      name,
      label: label || null,
    });
    if (!res.ok) {
      toast(res.error ?? "更新失败");
      return;
    }
    editingClient = null;
    toast(`已更新为「${res.name ?? name}」`);
    await refreshClients();
    await refreshStats();
  } catch (e) {
    toast(`更新失败：${String(e)}`);
  }
}

async function deleteOfflineClient(name: string) {
  const ok = await openConfirmModal(
    `确定删除离线后端「${name}」？删除后需重新注册才能再次接入。`,
    "删除离线后端",
  );
  if (!ok) return;
  try {
    const res = await invoke<DeleteClientResult>("hub_delete_client", { name });
    if (!res.ok) {
      toast(res.error ?? "删除失败");
      return;
    }
    if (editingClient?.oldName === name) editingClient = null;
    toast("已删除离线后端");
    await refreshClients();
    await refreshStats();
  } catch (e) {
    toast(`删除失败：${String(e)}`);
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

function setActiveTab(which: "home" | "backends" | "bridge") {
  const tabHome = $<HTMLButtonElement>("#tab-home");
  const tabBack = $<HTMLButtonElement>("#tab-backends");
  const tabBridge = $<HTMLButtonElement>("#tab-bridge");
  const panelHome = $<HTMLElement>("#panel-home");
  const panelBack = $<HTMLElement>("#panel-backends");
  const panelBridge = $<HTMLElement>("#panel-bridge");
  if (!tabHome || !tabBack || !tabBridge || !panelHome || !panelBack || !panelBridge) return;

  const isHome = which === "home";
  const isBackends = which === "backends";
  const isBridge = which === "bridge";
  document.body.dataset.activeTab = which;
  tabHome.setAttribute("aria-selected", String(isHome));
  tabBack.setAttribute("aria-selected", String(isBackends));
  tabBridge.setAttribute("aria-selected", String(isBridge));
  panelHome.hidden = !isHome;
  panelBack.hidden = !isBackends;
  panelBridge.hidden = !isBridge;

  if (isBackends) void refreshClients();
  if (isBridge) void refreshBridgeConfig();
}

window.addEventListener("DOMContentLoaded", () => {
  document.body.dataset.activeTab = "home";
  void refreshHubInfo();
  void refreshDesktopSettings();

  $("#tab-home")?.addEventListener("click", () => setActiveTab("home"));
  $("#tab-backends")?.addEventListener("click", () => setActiveTab("backends"));
  $("#tab-bridge")?.addEventListener("click", () => setActiveTab("bridge"));

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

  $("#btn-copy-bridge-path")?.addEventListener("click", () => {
    if (bridgeConfigPath) void copyToClipboard(bridgeConfigPath, "配置路径已复制");
  });

  $("#btn-new-profile")?.addEventListener("click", () => newBridgeProfile());
  $("#btn-save-profile")?.addEventListener("click", () => void saveBridgeProfile());
  $("#btn-delete-profile")?.addEventListener("click", () => void deleteBridgeProfile());
  $("#bridge-template")?.addEventListener("change", () => setTemplateVisibility());
  $("#btn-add-env-var")?.addEventListener("click", () => addEnvVarRow());
  $("#bridge-profile-list")?.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(".bridge-profile-item");
    const id = btn?.dataset.profileId;
    if (!id) return;
    const profile = bridgeProfiles.find((p) => p.id === id);
    if (profile) fillBridgeProfileEditor(profile);
  });
  $("#btn-bridge-start")?.addEventListener("click", () => void startBridge());
  $("#btn-bridge-stop")?.addEventListener("click", () => void stopBridge());
  $("#btn-bridge-restart")?.addEventListener("click", () => void restartBridge());

  $("#confirm-modal")?.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    const action = target.closest<HTMLElement>("[data-confirm]")?.dataset.confirm;
    if (action === "ok") closeConfirmModal(true);
    else if (action === "cancel") closeConfirmModal(false);
  });

  $("#client-list")?.addEventListener("click", async (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("[data-action]");
    if (!btn) return;
    const item = btn.closest<HTMLLIElement>(".client-item");
    if (!item) return;
    const action = btn.dataset.action;
    const clientName = item.dataset.clientName;
    if (!clientName) return;

    if (action === "copy") {
      const vtoken = btn.dataset.vtoken;
      if (!vtoken) return;
      const ok = await copyToClipboard(envFor(vtoken), "连接配置已复制");
      if (ok) {
        btn.classList.add("copied");
        setTimeout(() => btn.classList.remove("copied"), 1200);
      }
      return;
    }

    if (action === "edit") {
      const label = item.querySelector<HTMLElement>(".client-label")?.textContent ?? null;
      startEditClient(clientName, label);
      return;
    }

    if (action === "cancel") {
      cancelEditClient();
      return;
    }

    if (action === "save") {
      await saveEditClient(item);
      return;
    }

    if (action === "delete") {
      await deleteOfflineClient(clientName);
    }
  });

  $("#btn-stop")?.addEventListener("click", async () => {
    // Stop is a destructive action but no longer the only option — the footer
    // exposes a Start button afterwards so an accidental click is recoverable.
    // Keep the confirmation lightweight: explain consequence + default cancel.
    const ok = window.confirm(
      "确定要停止本机 Hub 服务吗？停止后可在下方重新启动。\n\n微信中转将暂时不可用，已连接的后端会断开。",
    );
    if (!ok) return;
    try {
      await invoke("bridge_stop");
      await invoke("stop_hub");
      setHubState("stopped", "已发送停止指令…");
      const btnStop = $<HTMLButtonElement>("#btn-stop");
      if (btnStop) btnStop.disabled = true;
    } catch (err) {
      setError(String(err));
    }
  });

  $("#btn-start")?.addEventListener("click", async () => {
    const btnStart = $<HTMLButtonElement>("#btn-start");
    if (btnStart) btnStart.disabled = true;
    try {
      setHubState("starting", "正在重新启动 Hub…");
      await invoke("start_hub");
      void refreshHubInfo();
    } catch (err) {
      setError(String(err));
      setHubState("stopped", "启动失败，可重试。");
      if (btnStart) btnStart.disabled = false;
    }
  });

  $("#btn-save-port")?.addEventListener("click", async () => {
    const input = $<HTMLInputElement>("#listen-port-input");
    if (!input) return;
    const raw = input.value.trim();
    const n = Number(raw);
    if (!raw || !Number.isInteger(n) || n < 1 || n > 65535) {
      setPortMsg("请输入 1..=65535 之间的端口号");
      return;
    }
    setPortMsg(null);
    await applyListenPortChange(n);
  });

  $("#listen-port-input")?.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      $("#btn-save-port")?.dispatchEvent(new Event("click", { bubbles: true }));
    }
  });

  $("#port-conflict-modal")?.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    const action = target.closest<HTMLElement>("[data-port-conflict]")?.dataset.portConflict;
    if (action === "cancel") {
      closePortConflictModal(null);
      return;
    }
    if (action === "apply") {
      const input = $<HTMLInputElement>("#port-conflict-input");
      const msg = $<HTMLElement>("#port-conflict-msg");
      const raw = input?.value.trim() ?? "";
      const n = Number(raw);
      if (!raw || !Number.isInteger(n) || n < 1 || n > 65535) {
        if (msg) {
          msg.textContent = "请输入 1..=65535 之间的端口号";
          msg.hidden = false;
        }
        return;
      }
      if (msg) {
        msg.textContent = "";
        msg.hidden = true;
      }
      closePortConflictModal(n);
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
    stopBridgePolling();
    clearStatsUi();
    void refreshBridgeStatus();
    void refreshHubInfo();
    if (isPortBindError(ev.payload)) {
      void handleBindFailure(ev.payload);
    }
  });

  void listen("hub-stopped", () => {
    setHubState("stopped", "服务已停止");
    const btnStop = $<HTMLButtonElement>("#btn-stop");
    if (btnStop) btnStop.disabled = true;
    $("#hero-hub-url")?.setAttribute("hidden", "");
    hideQrModal();
    stopClientPolling();
    stopBridgePolling();
    clearStatsUi();
    void refreshBridgeStatus();
    renderClientsEmpty("服务已停止。");
    // Reset so autoStartBridgeIfReady can fire again when hub comes back.
    bridgeAutoStartAttempted = false;
    // If bridge was running, schedule a bridge_restart when hub recovers so
    // the manager reconnects to the (potentially new) hub URL.
    bridgeNeedsRestartOnHubRecovery = lastBridgeRunning;
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
