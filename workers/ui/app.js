"use strict";

const TOKEN_KEY = "endara-relay-token";
const PROPAGATION = "All regions update within about a minute.";

const state = {
  token: localStorage.getItem(TOKEN_KEY) || "",
  config: null,
  status: {}, // server id -> { status, tools?, error? }
  statusLoading: false,
  open: null, // { kind: "server" | "profile", id } ; id "new" for drafts
  draft: null,
  test: null, // { loading, ok, error, tools }
  confirmDelete: false,
  saving: false,
  justOpened: false, // play the open animation once, not on every render
};

// ---- DOM helpers --------------------------------------------------------

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "value") el.value = v;
    else if (k === "checked") el.checked = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

let toastTimer;
function toast(message, isError = false) {
  const el = document.getElementById("toast");
  el.textContent = message;
  el.className = isError ? "show error" : "show";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.className = isError ? "error" : ""), isError ? 6000 : 3500);
}

async function copy(text, label) {
  try {
    await navigator.clipboard.writeText(text);
    toast(`${label} copied`);
  } catch {
    toast("Copy failed", true);
  }
}

// ---- API ----------------------------------------------------------------

async function api(method, path, body) {
  const res = await fetch(`/api/${path}`, {
    method,
    headers: { authorization: `Bearer ${state.token}`, ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) {
    signOut("That token was not accepted.");
    throw new Error("unauthorized");
  }
  if (!res.ok) {
    const err = new Error(data.error || `Request failed (${res.status})`);
    err.status = res.status;
    throw err;
  }
  return data;
}

async function loadAll() {
  state.config = await api("GET", "config");
  render();
  refreshStatus(false);
}

async function refreshStatus(fresh) {
  state.statusLoading = true;
  render();
  try {
    const data = await api("GET", `status${fresh ? "?fresh=1" : ""}`);
    state.status = Object.fromEntries(data.servers.map((s) => [s.id, s]));
  } catch (e) {
    if (e.message !== "unauthorized") toast(e.message, true);
  } finally {
    state.statusLoading = false;
    render();
  }
}

/// Send the whole config. Server entries without a header value keep the
/// stored secret, so we only ever send values the user typed.
async function saveConfig(next, message = "Saved") {
  state.saving = true;
  render();
  try {
    state.config = await api("PUT", "config", { ...next, revision: state.config.revision });
    toast(`${message}. ${PROPAGATION}`);
    return true;
  } catch (e) {
    if (e.status === 409) {
      toast(e.message, true);
      state.config = await api("GET", "config");
    } else if (e.message !== "unauthorized") {
      toast(e.message, true);
    }
    return false;
  } finally {
    state.saving = false;
    render();
  }
}

function plainConfig() {
  // Saved headers go back without values (keep), as returned by GET.
  const c = state.config;
  return {
    settings: { ...c.settings },
    endpoints: c.endpoints.map((e) => ({ ...e, headers: e.headers.map((hd) => ({ name: hd.name })) })),
    profiles: c.profiles.map((p) => ({ ...p })),
  };
}

// ---- Sign in ------------------------------------------------------------

function signOut(message) {
  localStorage.removeItem(TOKEN_KEY);
  Object.assign(state, { token: "", config: null, status: {}, open: null, draft: null, test: null });
  renderLogin(message);
}

function renderLogin(message) {
  const input = h("input", { type: "password", placeholder: "Bearer token", autocomplete: "current-password", "aria-label": "Bearer token" });
  const error = h("div", { class: "note error" }, message || "");
  const button = h("button", { class: "btn primary", type: "submit" }, "Sign in");
  const form = h(
    "form",
    {
      class: "card",
      onsubmit: async (ev) => {
        ev.preventDefault();
        const token = input.value.trim().replace(/^bearer\s+/i, "");
        if (!token) return;
        button.disabled = true;
        state.token = token;
        try {
          state.config = await api("GET", "config");
          localStorage.setItem(TOKEN_KEY, token);
          render();
          refreshStatus(false);
        } catch (e) {
          if (e.message !== "unauthorized") error.textContent = e.message;
          button.disabled = false;
        }
      },
    },
    h("h1", {}, "Endara Relay"),
    h("p", {}, "Paste the relay's bearer token to sign in."),
    input,
    button,
    error,
  );
  document.getElementById("app").replaceChildren(h("div", { class: "login" }, form));
  input.focus();
}

// ---- Server editor ------------------------------------------------------

function authModeOf(headers) {
  if (headers.length === 0) return "none";
  if (headers.length === 1 && /^authorization$/i.test(headers[0].name) && /^Bearer /.test(headers[0].preview || "")) return "bearer";
  return "headers";
}

function openServer(id) {
  const existing = state.config.endpoints.find((e) => e.id === id);
  const src = existing || { id: "", name: "", url: "", description: "", tool_prefix: "", headers: [], enabled: true, disabled_tools: [] };
  const mode = authModeOf(src.headers);
  state.open = { kind: "server", id: existing ? id : "new" };
  state.justOpened = true;
  state.confirmDelete = false;
  state.test = null;
  state.draft = {
    id: src.id,
    name: src.name,
    url: src.url,
    description: src.description || "",
    tool_prefix: src.tool_prefix || "",
    enabled: src.enabled,
    disabled_tools: [...src.disabled_tools],
    authMode: mode,
    bearer: "",
    bearerPreview: mode === "bearer" ? src.headers[0].preview : "",
    headers: mode === "headers" ? src.headers.map((hd) => ({ name: hd.name, value: "", preview: hd.preview, saved: true })) : [],
  };
  render();
}

function closeEditor() {
  state.open = null;
  state.draft = null;
  state.test = null;
  render();
}

function draftHeaders(d) {
  if (d.authMode === "bearer") {
    if (d.bearer.trim()) return [{ name: "Authorization", value: `Bearer ${d.bearer.trim().replace(/^bearer\s+/i, "")}` }];
    return d.bearerPreview ? [{ name: "Authorization" }] : [];
  }
  if (d.authMode === "headers") {
    return d.headers
      .filter((x) => x.name.trim())
      .map((x) => (x.value ? { name: x.name.trim(), value: x.value } : x.saved ? { name: x.name.trim() } : { name: x.name.trim(), value: "" }));
  }
  return [];
}

async function testDraft() {
  const d = state.draft;
  state.test = { loading: true };
  render();
  try {
    const r = await api("POST", "test", { id: d.id || undefined, url: d.url, headers: draftHeaders(d) });
    state.test = r.ok ? { ok: true, tools: r.tools, more: r.more } : { ok: false, error: r.error };
  } catch (e) {
    state.test = { ok: false, error: e.message };
  }
  render();
}

async function saveServer() {
  const d = state.draft;
  if (d.authMode === "bearer" && !d.bearer.trim() && !d.bearerPreview) {
    toast("Enter the bearer token, or choose None", true);
    return;
  }
  const next = plainConfig();
  const entry = {
    id: d.id,
    name: d.name.trim(),
    url: d.url.trim(),
    description: d.description.trim() || null,
    tool_prefix: d.tool_prefix.trim() || null,
    headers: draftHeaders(d),
    enabled: d.enabled,
    disabled_tools: d.disabled_tools,
  };
  if (d.id) next.endpoints = next.endpoints.map((e) => (e.id === d.id ? entry : e));
  else next.endpoints.push(entry);
  if (await saveConfig(next, d.id ? "Server saved" : "Server added")) {
    closeEditor();
    refreshStatus(true);
  }
}

async function deleteServer() {
  const id = state.draft.id;
  const next = plainConfig();
  next.endpoints = next.endpoints.filter((e) => e.id !== id);
  next.profiles = next.profiles.map((p) => ({ ...p, endpoint_ids: p.endpoint_ids.filter((x) => x !== id) }));
  if (await saveConfig(next, "Server removed")) closeEditor();
}

async function toggleServer(id, enabled) {
  const next = plainConfig();
  next.endpoints = next.endpoints.map((e) => (e.id === id ? { ...e, enabled } : e));
  if (await saveConfig(next, enabled ? "Server enabled" : "Server disabled")) refreshStatus(false);
}

function serverRow(ep) {
  const st = state.status[ep.id];
  let dot = "dot loading";
  let meta = h("span", { class: "meta" }, "");
  if (!ep.enabled) {
    dot = "dot";
    meta = h("span", { class: "meta" }, "Off");
  } else if (st && st.status === "ok") {
    dot = "dot ok";
    const on = st.tools.filter((t) => !ep.disabled_tools.includes(t.name)).length;
    const off = st.tools.length - on;
    meta = h("span", { class: "meta" }, `${on} tool${on === 1 ? "" : "s"}${off ? ` · ${off} off` : ""}`);
  } else if (st && st.status === "error") {
    dot = "dot error";
    meta = h("span", { class: "meta error", title: st.error }, st.error);
  } else if (!state.statusLoading) {
    dot = "dot";
  }
  const isOpen = state.open && state.open.kind === "server" && state.open.id === ep.id;
  return h(
    "div",
    { class: `item${isOpen ? " open" : ""}` },
    h(
      "div",
      {
        class: "row clickable",
        role: "button",
        tabindex: "0",
        "aria-expanded": isOpen ? "true" : "false",
        onclick: () => (isOpen ? closeEditor() : openServer(ep.id)),
        onkeydown: (ev) => {
          if (ev.key === "Enter" || ev.key === " ") {
            ev.preventDefault();
            isOpen ? closeEditor() : openServer(ep.id);
          }
        },
      },
      h("span", { class: dot }),
      h("div", { class: "main" }, h("div", { class: "title" }, ep.name), h("div", { class: "sub" }, ep.url)),
      meta,
      h("button", {
        class: "switch",
        role: "switch",
        "aria-checked": ep.enabled ? "true" : "false",
        "aria-label": `${ep.enabled ? "Disable" : "Enable"} ${ep.name}`,
        disabled: state.saving,
        onclick: (ev) => {
          ev.stopPropagation();
          toggleServer(ep.id, !ep.enabled);
        },
        onkeydown: (ev) => ev.stopPropagation(),
      }),
    ),
    isOpen ? serverEditor() : null,
  );
}

function field(label, input) {
  return h("div", { class: "field" }, h("label", {}, label), input);
}

function bind(key, attrs = {}) {
  return h("input", {
    type: "text",
    "aria-label": key,
    ...attrs,
    value: state.draft[key],
    oninput: (ev) => (state.draft[key] = ev.target.value),
  });
}

function serverEditor() {
  const d = state.draft;
  const isNew = !d.id;
  const seg = h(
    "div",
    { class: "seg", role: "group", "aria-label": "Authentication" },
    ...[
      ["none", "None"],
      ["bearer", "Bearer token"],
      ["headers", "Headers"],
    ].map(([mode, label]) =>
      h(
        "button",
        {
          type: "button",
          "aria-pressed": d.authMode === mode ? "true" : "false",
          onclick: () => {
            d.authMode = mode;
            if (mode === "headers" && d.headers.length === 0) d.headers.push({ name: "", value: "", saved: false });
            render();
          },
        },
        label,
      ),
    ),
  );

  let auth = null;
  if (d.authMode === "bearer") {
    auth = h("input", {
      type: "password",
      autocomplete: "off",
      "aria-label": "Bearer token",
      placeholder: d.bearerPreview ? `${d.bearerPreview}  (leave empty to keep)` : "Paste the token",
      value: d.bearer,
      oninput: (ev) => (d.bearer = ev.target.value),
    });
  } else if (d.authMode === "headers") {
    auth = h(
      "div",
      { class: "headers" },
      ...d.headers.map((row, i) =>
        h(
          "div",
          { class: "header-row" },
          h("input", { type: "text", placeholder: "Header", "aria-label": "Header name", value: row.name, oninput: (ev) => (row.name = ev.target.value) }),
          h("input", {
            type: "password",
            autocomplete: "off",
            "aria-label": "Header value",
            placeholder: row.saved ? `${row.preview}  (leave empty to keep)` : "Value",
            value: row.value,
            oninput: (ev) => (row.value = ev.target.value),
          }),
          h("button", { type: "button", class: "btn quiet", "aria-label": "Remove header", onclick: () => (d.headers.splice(i, 1), render()) }, "Remove"),
        ),
      ),
      h("div", {}, h("button", { type: "button", class: "btn quiet", onclick: () => (d.headers.push({ name: "", value: "", saved: false }), render()) }, "Add header")),
    );
  }

  // Tools: from a fresh test, else from the last status for this server.
  const st = state.status[d.id];
  const tools = state.test && state.test.ok ? state.test.tools : st && st.status === "ok" ? st.tools : null;
  let toolsBlock = null;
  if (tools && tools.length) {
    const enabledCount = tools.filter((t) => !d.disabled_tools.includes(t.name)).length;
    toolsBlock = h(
      "div",
      { class: "field" },
      h(
        "div",
        { class: "tools-head" },
        h("label", {}, "Tools"),
        h("span", { class: "muted" }, `${enabledCount} of ${tools.length} on`),
      ),
      h(
        "div",
        { class: "tools" },
        ...tools.map((t) =>
          h(
            "label",
            { class: "tool" },
            h("input", {
              type: "checkbox",
              checked: !d.disabled_tools.includes(t.name),
              onchange: (ev) => {
                d.disabled_tools = ev.target.checked ? d.disabled_tools.filter((x) => x !== t.name) : [...d.disabled_tools, t.name];
                render();
              },
            }),
            h("div", {}, h("div", { class: "name" }, t.name), t.description ? h("div", { class: "desc" }, t.description) : null),
          ),
        ),
      ),
    );
  }

  let testNote = null;
  if (state.test && state.test.loading) testNote = h("span", { class: "note" }, "Connecting…");
  else if (state.test && state.test.ok) testNote = h("span", { class: "note ok" }, `Connected · ${state.test.tools.length}${state.test.more ? "+" : ""} tool${state.test.tools.length === 1 && !state.test.more ? "" : "s"}`);
  else if (state.test) testNote = h("span", { class: "note error" }, state.test.error);

  const form = h(
    "form",
    {
      class: state.justOpened ? "editor enter" : "editor",
      onsubmit: (ev) => {
        ev.preventDefault();
        saveServer();
      },
      onclick: (ev) => ev.stopPropagation(),
    },
    h("div", { class: "grid2" }, field("Name", bind("name", { placeholder: "github", required: true })), field("Label (optional)", bind("description", { placeholder: "Shown before each tool description" }))),
    field("URL", bind("url", { type: "url", placeholder: "https://example.com/mcp", required: true })),
    field("Authentication", h("div", { class: "headers" }, seg, auth)),
    field("Tool prefix (optional)", bind("tool_prefix", { placeholder: (d.name && d.name.toLowerCase().replace(/ /g, "_").replace(/[^a-z0-9_-]/g, "")) || "defaults to the name" })),
    toolsBlock,
    h(
      "div",
      { class: "actions" },
      h("button", { type: "submit", class: "btn primary", disabled: state.saving }, isNew ? "Add server" : "Save"),
      h("button", { type: "button", class: "btn", disabled: !!(state.test && state.test.loading), onclick: testDraft }, "Test connection"),
      h("button", { type: "button", class: "btn quiet", onclick: closeEditor }, "Cancel"),
      testNote,
      h("span", { class: "spacer" }),
      isNew
        ? null
        : h(
            "button",
            {
              type: "button",
              class: "btn danger",
              disabled: state.saving,
              onclick: () => {
                if (state.confirmDelete) deleteServer();
                else {
                  state.confirmDelete = true;
                  render();
                }
              },
            },
            state.confirmDelete ? "Click again to remove" : "Remove",
          ),
    ),
  );
  return form;
}

// ---- Profiles -------------------------------------------------------------

function openProfile(id) {
  const existing = state.config.profiles.find((p) => p.id === id);
  const src = existing || { id: "", name: "", path: "", endpoint_ids: [], toon_output: null };
  state.open = { kind: "profile", id: existing ? id : "new" };
  state.justOpened = true;
  state.confirmDelete = false;
  state.draft = { ...src, endpoint_ids: [...src.endpoint_ids] };
  render();
}

async function saveProfile() {
  const d = state.draft;
  const next = plainConfig();
  const entry = { id: d.id, name: d.name.trim() || d.path.trim(), path: d.path.trim(), endpoint_ids: d.endpoint_ids, toon_output: d.toon_output };
  if (d.id) next.profiles = next.profiles.map((p) => (p.id === d.id ? entry : p));
  else next.profiles.push(entry);
  if (await saveConfig(next, d.id ? "Profile saved" : "Profile added")) closeEditor();
}

async function deleteProfile() {
  const next = plainConfig();
  next.profiles = next.profiles.filter((p) => p.id !== state.draft.id);
  if (await saveConfig(next, "Profile removed")) closeEditor();
}

function profileRow(p) {
  const isOpen = state.open && state.open.kind === "profile" && state.open.id === p.id;
  const url = `${location.origin}/mcp/${p.path}`;
  const count = p.endpoint_ids.length;
  return h(
    "div",
    { class: `item${isOpen ? " open" : ""}` },
    h(
      "div",
      {
        class: "row clickable",
        role: "button",
        tabindex: "0",
        "aria-expanded": isOpen ? "true" : "false",
        onclick: () => (isOpen ? closeEditor() : openProfile(p.id)),
        onkeydown: (ev) => {
          if (ev.key === "Enter" || ev.key === " ") {
            ev.preventDefault();
            isOpen ? closeEditor() : openProfile(p.id);
          }
        },
      },
      h("div", { class: "main" }, h("div", { class: "title" }, p.name), h("div", { class: "sub mono" }, url)),
      h("span", { class: "meta" }, `${count} server${count === 1 ? "" : "s"}`),
      h(
        "button",
        {
          class: "btn quiet",
          type: "button",
          onclick: (ev) => {
            ev.stopPropagation();
            copy(url, "Profile URL");
          },
        },
        "Copy URL",
      ),
    ),
    isOpen ? profileEditor() : null,
  );
}

function profileEditor() {
  const d = state.draft;
  const isNew = !d.id;
  const toonMode = d.toon_output === null || d.toon_output === undefined ? "inherit" : d.toon_output ? "on" : "off";
  return h(
    "form",
    {
      class: state.justOpened ? "editor enter" : "editor",
      onsubmit: (ev) => {
        ev.preventDefault();
        saveProfile();
      },
      onclick: (ev) => ev.stopPropagation(),
    },
    h("div", { class: "grid2" }, field("Name", bind("name", { placeholder: "Research" })), field("Path", bind("path", { placeholder: "research", required: true, pattern: "[A-Za-z0-9_-]+" }))),
    field(
      "Servers",
      state.config.endpoints.length
        ? h(
            "div",
            { class: "checks" },
            ...state.config.endpoints.map((ep) =>
              h(
                "label",
                { class: "chip" },
                h("input", {
                  type: "checkbox",
                  checked: d.endpoint_ids.includes(ep.id),
                  onchange: (ev) => (d.endpoint_ids = ev.target.checked ? [...d.endpoint_ids, ep.id] : d.endpoint_ids.filter((x) => x !== ep.id)),
                }),
                ep.name,
              ),
            ),
          )
        : h("span", { class: "note" }, "Add a server first."),
    ),
    field(
      "TOON output",
      h(
        "div",
        { class: "seg", role: "group", "aria-label": "TOON output" },
        ...[
          ["inherit", "Use setting"],
          ["on", "On"],
          ["off", "Off"],
        ].map(([mode, label]) =>
          h(
            "button",
            {
              type: "button",
              "aria-pressed": toonMode === mode ? "true" : "false",
              onclick: () => {
                d.toon_output = mode === "inherit" ? null : mode === "on";
                render();
              },
            },
            label,
          ),
        ),
      ),
    ),
    h(
      "div",
      { class: "actions" },
      h("button", { type: "submit", class: "btn primary", disabled: state.saving }, isNew ? "Add profile" : "Save"),
      h("button", { type: "button", class: "btn quiet", onclick: closeEditor }, "Cancel"),
      h("span", { class: "spacer" }),
      isNew
        ? null
        : h(
            "button",
            {
              type: "button",
              class: "btn danger",
              onclick: () => {
                if (state.confirmDelete) deleteProfile();
                else {
                  state.confirmDelete = true;
                  render();
                }
              },
            },
            state.confirmDelete ? "Click again to remove" : "Remove",
          ),
    ),
  );
}

// ---- Settings -------------------------------------------------------------

function settingsCard() {
  const s = state.config.settings;
  const commit = (patch) => {
    const next = plainConfig();
    next.settings = { ...next.settings, ...patch };
    saveConfig(next, "Settings saved");
  };
  const numberInput = (key, min, max) =>
    h("input", {
      type: "number",
      min,
      max,
      value: s[key],
      "aria-label": key,
      onchange: (ev) => {
        const v = Number(ev.target.value);
        if (Number.isInteger(v) && v >= min && v <= max && v !== s[key]) commit({ [key]: v });
        else ev.target.value = s[key];
      },
    });
  return h(
    "div",
    { class: "card list settings" },
    h(
      "div",
      { class: "item" },
      h(
        "div",
        { class: "row" },
        h("div", { class: "main" }, "Relay name"),
        h("input", {
          type: "text",
          value: s.machine_name,
          "aria-label": "Relay name",
          onchange: (ev) => ev.target.value.trim() && ev.target.value.trim() !== s.machine_name && commit({ machine_name: ev.target.value.trim() }),
        }),
      ),
    ),
    h(
      "div",
      { class: "item" },
      h(
        "div",
        { class: "row" },
        h("div", { class: "main" }, h("div", {}, "TOON output"), h("div", { class: "sub" }, "Compact tool results that use fewer tokens")),
        h("button", {
          class: "switch",
          role: "switch",
          "aria-checked": s.toon_output ? "true" : "false",
          "aria-label": "TOON output",
          disabled: state.saving,
          onclick: () => commit({ toon_output: !s.toon_output }),
        }),
      ),
    ),
    h(
      "div",
      { class: "item" },
      h("div", { class: "row" }, h("div", { class: "main" }, h("div", {}, "Tool list cache"), h("div", { class: "sub" }, "Seconds before tool lists are fetched again")), numberInput("cache_ttl_secs", 0, 86400)),
    ),
    h(
      "div",
      { class: "item" },
      h("div", { class: "row" }, h("div", { class: "main" }, h("div", {}, "Request timeout"), h("div", { class: "sub" }, "Seconds to wait for a server")), numberInput("request_timeout_secs", 1, 300)),
    ),
  );
}

// ---- Page ---------------------------------------------------------------

function render() {
  if (!state.config) return;
  const app = document.getElementById("app");
  const active = document.activeElement;
  const focusKey = active && active.getAttribute && active.getAttribute("aria-label");
  const caret = active && typeof active.selectionStart === "number" ? [active.selectionStart, active.selectionEnd] : null;
  const c = state.config;
  const mcpUrl = `${location.origin}/mcp`;

  const servers = [...c.endpoints.map(serverRow)];
  if (state.open && state.open.kind === "server" && state.open.id === "new") {
    servers.push(h("div", { class: "item open" }, h("div", { class: "row" }, h("span", { class: "dot" }), h("div", { class: "main title" }, "New server")), serverEditor()));
  }
  const profiles = [...c.profiles.map(profileRow)];
  if (state.open && state.open.kind === "profile" && state.open.id === "new") {
    profiles.push(h("div", { class: "item open" }, h("div", { class: "row" }, h("div", { class: "main title" }, "New profile")), profileEditor()));
  }

  app.replaceChildren(
    h(
      "div",
      { class: "wrap" },
      h(
        "header",
        { class: "topbar" },
        h("h1", {}, "Endara Relay"),
        h("span", { class: "machine" }, c.settings.machine_name),
        h("span", { class: "spacer" }),
        h("button", { class: "btn quiet", onclick: () => signOut() }, "Sign out"),
      ),
      h(
        "div",
        { class: "card list connect" },
        h(
          "div",
          { class: "item" },
          h(
            "div",
            { class: "row" },
            h("span", { class: "label" }, "URL"),
            h("span", { class: "main mono" }, mcpUrl),
            h("button", { class: "btn quiet", onclick: () => copy(mcpUrl, "URL") }, "Copy"),
          ),
        ),
        h(
          "div",
          { class: "item" },
          h(
            "div",
            { class: "row" },
            h("span", { class: "label" }, "Header"),
            h("span", { class: "main mono" }, "Authorization: Bearer ••••••••"),
            h("button", { class: "btn quiet", onclick: () => copy(`Bearer ${state.token}`, "Header value") }, "Copy"),
          ),
        ),
      ),
      h(
        "section",
        {},
        h(
          "div",
          { class: "section-head" },
          h("h2", {}, "Servers"),
          h(
            "div",
            { class: "actions" },
            c.endpoints.length
              ? h("button", { class: "btn quiet", disabled: state.statusLoading, onclick: () => refreshStatus(true) }, state.statusLoading ? "Checking…" : "Refresh")
              : null,
            h("button", { class: "btn", onclick: () => openServer(null), disabled: !!(state.open && state.open.id === "new") }, "Add server"),
          ),
        ),
        h(
          "div",
          { class: "card list" },
          servers.length ? servers : h("div", { class: "empty" }, "No servers yet. Add a remote MCP server to start."),
        ),
      ),
      h(
        "section",
        {},
        h(
          "div",
          { class: "section-head" },
          h("h2", {}, "Profiles"),
          h("button", { class: "btn", onclick: () => openProfile(null), disabled: !!(state.open && state.open.id === "new") }, "Add profile"),
        ),
        h("div", { class: "card list" }, profiles.length ? profiles : h("div", { class: "empty" }, "Profiles serve a subset of servers at their own URL.")),
      ),
      h("section", {}, h("div", { class: "section-head" }, h("h2", {}, "Settings")), settingsCard()),
    ),
  );

  state.justOpened = false;
  if (focusKey) {
    const again = app.querySelector(`[aria-label="${CSS.escape(focusKey)}"]`);
    if (again && again !== document.activeElement) {
      again.focus();
      if (caret && typeof again.setSelectionRange === "function") {
        try {
          again.setSelectionRange(caret[0], caret[1]);
        } catch {}
      }
    }
  }
}

if (state.token) {
  loadAll().catch((e) => {
    if (e.message !== "unauthorized") renderLogin(e.message);
  });
} else {
  renderLogin();
}
