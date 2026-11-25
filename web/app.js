const $ = (s, root = document) => root.querySelector(s);
const main = $("#main"),
  loadingIndicator = $(".router-loading"),
  dialog = $("#dialog"),
  form = $("#form");
const names = { claude: "Claude", codex: "Codex", agy: "Antigravity" };
const themeKey = "ccp_theme";
const themeButton = $("#theme");
const themeModes = {
  system: {icon: "monitor", next: "light"},
  light: {icon: "sun", next: "dark"},
  dark: {icon: "moon", next: "system"},
};
function applyTheme(preference) {
  const value = Object.hasOwn(themeModes, preference) ? preference : "system";
  document.documentElement.dataset.theme = value;
  const {icon, next} = themeModes[value];
  $("use", themeButton).setAttribute("href", "/icons.svg#" + icon);
  themeButton.title = "Theme: " + value + ". Switch to " + next + ".";
  themeButton.setAttribute("aria-label", themeButton.title);
}
applyTheme(localStorage.getItem(themeKey));
themeButton.onclick = () => {
  const {next} = themeModes[document.documentElement.dataset.theme];
  applyTheme(next);
  if (next === "system") localStorage.removeItem(themeKey);
  else localStorage.setItem(themeKey, next);
};
window.addEventListener("storage", (e) => {
  if (e.key === themeKey || e.key === null) applyTheme(localStorage.getItem(themeKey));
});
const states = {
  routed: "Routed",
  pending: "Awaiting ID",
  observed: "ID observed",
  denied: "Denied",
  conflict: "Conflict",
  unobserved: "ID not observed",
  exec_failed: "Exec failed",
};
const ready = {
  ready: "Ready",
  path_missing: "Directory missing or replaced",
  unsupported_build: "Native executable unavailable or mismatched",
};
const routingModes = {
  earliest_expiry_most_remaining: "Earliest expiry, most quota remaining",
  earliest_expiry_least_remaining: "Earliest expiry, least quota remaining",
  most_remaining: "Most quota remaining",
  least_remaining: "Least quota remaining",
  round_robin: "Round robin",
  priority: "Highest priority",
};
const decisionLabels = {
  ...routingModes,
  earliest_expiry: "Earliest expiry (previous mode)",
  current: "Default account",
  no_current: "No default account",
  current_missing: "Default account directory missing or replaced",
};
const quotaStates = {
  not_checked: "Not checked",
  sign_in_required: "Sign-in required",
  rate_limited: "Rate limited",
  unavailable: "Unavailable",
  fresh: "Checked",
};
let sessionToken = sessionStorage.getItem("ccp_session") || "",
  needsSetup = false,
  version = 0,
  cursors = [null],
  timer,
  toastTimer,
  lastView = "",
  expanded = null;

function el(tag, props = {}, ...children) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === false || v === null || v === undefined) continue;
    if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else if (k === "class") n.className = v;
    else n.setAttribute(k, v === true ? "" : String(v));
  }
  n.append(
    ...children
      .flat(Infinity)
      .filter((v) => v != null && v !== false)
      .map((v) => (v instanceof Node ? v : String(v))),
  );
  return n;
}
function icon(name) {
  const ns = "http://www.w3.org/2000/svg",
    svg = document.createElementNS(ns, "svg"),
    use = document.createElementNS(ns, "use");
  svg.setAttribute("aria-hidden", "true");
  use.setAttribute("href", "/icons.svg#" + name);
  svg.append(use);
  return svg;
}
const btn = (label, fn, glyph, cls = "") =>
  el("button", { type: "button", class: cls, onclick: guard(fn) }, glyph && icon(glyph), label);
const ib = (label, glyph, fn, disabled = false) =>
  el(
    "button",
    {
      type: "button",
      class: "icon",
      title: label,
      "aria-label": label,
      disabled,
      onclick: guard(fn),
    },
    icon(glyph),
  );
const pill = (label, color = "") => el("span", { class: "pill " + color }, label);
const code = (s) => el("code", {}, s);
const text = (s, cls = "muted") => el("span", { class: cls }, s);
const datum = (label, v) => [el("dt", {}, label), el("dd", {}, v)];
const fmt = (ms) =>
  ms == null
    ? "Unknown"
    : new Date(ms).toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
const simple = (s) => s.replaceAll("_", " ");
const percent = (n) => Number(n.toFixed(1)) + "%";
const shell = (s) => "'" + s.replaceAll("'", "'\"'\"'") + "'";
function notice(message) {
  const n = $("#notice");
  n.hidden = false;
  n.textContent = message;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (n.hidden = true), 5500);
}
function guard(fn) {
  return async (e) => {
    try {
      await fn(e);
    } catch (err) {
      notice(err.message);
    }
  };
}
async function api(path, method = "GET", body, signal) {
  const token = sessionToken;
  const opts = {
    method,
    credentials: "omit",
    signal,
    headers: { "Content-Type": "application/json", "X-CCP-Session": token },
  };
  if (body !== undefined) opts.body = JSON.stringify(body);
  const r = await fetch("/api/v1/" + path, opts);
  const v = await r.json().catch(() => ({}));
  if (!r.ok) {
    if (r.status === 403 && token && token === sessionToken) signedOut();
    throw Object.assign(new Error(v.message || "Request failed (" + r.status + ")."), {code: v.error});
  }
  return v;
}
function locationState() {
  const [v, q] = location.hash.slice(1).split("?");
  return {
    view: ["accounts", "sessions", "settings"].includes(v) ? v : "dashboard",
    params: new URLSearchParams(q),
  };
}
function go(view, obj = {}) {
  const q = new URLSearchParams(Object.entries(obj).filter(([, v]) => v != null && v !== ""));
  location.hash = view + (q.size ? "?" + q : "");
}
const field = (label, control) => el("label", { class: "field" }, label, control);
function refreshControl(label, action) {
  let pending = false, checking = false, blocked = false;
  const button = ib(label, "refresh-cw", () => run());
  function sync() {
    button.setAttribute("aria-busy", String(pending || checking));
    button.setAttribute("aria-disabled", String(pending || blocked));
  }
  async function run(...args) {
    if (pending || blocked) return;
    pending = true;
    sync();
    try { await action(...args); }
    finally { pending = false; sync(); }
  }
  // A queued quota check can outlive the HTTP request that started it.
  function update(isChecking, isBlocked = isChecking) {
    checking = isChecking;
    blocked = isBlocked;
    sync();
  }
  return {button, run, update};
}
function select(items, val, props = {}) {
  const box = el("select", props, ...items.map(([v, name]) => el("option", { value: v }, name)));
  box.value = val;
  return box;
}
function modal(heading, fields, submit, fn) {
  $(".dialog-head h2", form).textContent = heading;
  $(".fields", form).replaceChildren(...fields);
  const save = $("button[type=submit]", form),
    err = $(".error", form);
  $(".cancel", form).hidden = !fn;
  save.textContent = submit;
  save.disabled = false;
  err.hidden = true;
  form.onsubmit = async (e) => {
    e.preventDefault();
    if (!fn) return dialog.close();
    save.disabled = true;
    err.hidden = true;
    try {
      await fn();
      dialog.close();
      await render();
    } catch (ex) {
      err.textContent = ex.message;
      err.hidden = false;
    } finally {
      save.disabled = false;
    }
  };
  dialog.showModal();
}
$(".cancel", form).onclick = $(".close", form).onclick = () => dialog.close();
const copy = async (s) => {
  const token = sessionToken, route = location.hash;
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(s);
      return notice("Copied to clipboard.");
    }
  } catch {
    // A browser can expose the clipboard API but deny writes.
  }
  if (!token || sessionToken !== token || route !== location.hash || dialog.open) return;
  const focus = document.activeElement;
  const box = el("textarea", {readonly: true, class: "clipboard-buffer", "aria-label": "Text to copy", rows: 3});
  box.value = s;
  document.body.append(box);
  let copied = false;
  try {
    box.select();
    copied = document.execCommand("copy");
  } catch {
    // Manual copy also works when both browser mechanisms are unavailable.
  } finally {
    box.remove();
    focus?.focus({preventScroll: true});
  }
  if (copied) return notice("Copied to clipboard.");
  box.className = "manual-copy";
  modal("Copy manually", [box], "Done");
  box.focus();
  box.select();
  dialog.addEventListener("close", () => box.remove(), {once: true});
};
const passwordInput = (name, autocomplete, props = {}) => el("input", {
  type: "password", name, required: true, maxlength: 1024, autocomplete, spellcheck: "false", ...props,
});
function passwordForm() {
  resetDialog?.close();
  resetAttempts.clear();
  readDashboardQuotas = null;
  selectedIdentity = null;
  accountQuota = null;
  accountRefresh = null;
  clearTimeout(timer);
  version++;
  $("nav").hidden = true;
  $(".logout").hidden = true;
  if (dialog.open) dialog.close();
  const setup = needsSetup, heading = setup ? "Set panel password" : "Sign in";
  main.removeAttribute("aria-label");
  document.title = heading + " | cc-profile";
  const input = (name) => passwordInput(name, setup ? "new-password" : "current-password");
  const password = input("password"), confirm = setup ? input("confirmation") : null;
  if (setup) password.setAttribute("aria-describedby", "password-hint");
  const submit = el("button", {type: "submit", class: "primary"}, setup ? "Set password" : "Sign in");
  const error = el("p", { class: "error", role: "alert", hidden: true });
  const f = el(
    "form",
    { class: "password-form" },
    el("h1", {}, heading),
    setup && el("p", { id: "password-hint", class: "muted" }, "Use at least 4 characters."),
    field(setup ? "New password" : "Password", password),
    confirm && field("Confirm password", confirm),
    error,
    submit,
  );
  f.onsubmit = async (e) => {
    e.preventDefault();
    submit.disabled = true;
    try {
      if (confirm && password.value !== confirm.value) throw new Error("Passwords do not match.");
      sessionToken = (await api(setup ? "setup" : "login", "POST", {password: password.value})).session;
      sessionStorage.setItem("ccp_session",sessionToken);
      password.value = "";
      if (confirm) confirm.value = "";
      needsSetup = false;
      lastView = "";
      await render();
    } catch (err) {
      if (err.code === "password_set" || err.code === "setup_required") {
        needsSetup = err.code === "setup_required";
        passwordForm();
        notice(err.message);
        return;
      }
      error.textContent = err.message;
      error.hidden = false;
    } finally { submit.disabled = false; }
  };
  main.replaceChildren(f);
}
function signedOut() {
  sessionToken = "";
  sessionStorage.removeItem("ccp_session");
  passwordForm();
}
$(".logout").onclick = guard(async () => {
  await api("logout", "POST", {});
  signedOut();
});
function changePasswordForm() {
  const current = passwordInput("current_password", "current-password"),
    next = passwordInput("new_password", "new-password", {minlength: 4, "aria-describedby": "change-hint"}),
    confirm = passwordInput("confirmation", "new-password");
  const submit = el("button", {type: "submit", class: "primary"}, "Change password"),
    error = el("p", {class: "error", role: "alert", hidden: true});
  return el("form", {onsubmit: async (e) => {
    e.preventDefault();
    submit.disabled = true;
    error.hidden = true;
    try {
      if ([...next.value].length < 4) throw new Error("Use at least 4 characters.");
      if (next.value !== confirm.value) throw new Error("Passwords do not match.");
      const submittingToken = sessionToken;
      await api("password", "POST", {current_password: current.value, new_password: next.value});
      if (sessionToken !== submittingToken) return;
      signedOut();
      notice("Password changed. Sign in again.");
    } catch (err) {
      error.textContent = err.message;
      error.hidden = false;
    } finally { submit.disabled = false; }
  }},
    field("Current password", current), field("New password", next), field("Confirm new password", confirm),
    el("p", {id: "change-hint", class: "caption"}, "Use at least 4 characters. Changing the password signs out all sessions."),
    error, submit);
}
async function loadRouting(container) {
  container.replaceChildren(text("Loading routing mode…"));
  try {
    const settings = await api("settings");
    const mode = select(Object.entries(routingModes), settings.routing_mode, {required: true});
    const save = el("button", {type: "submit"}, "Save routing mode");
    const error = el("p", {class: "error", role: "alert", hidden: true});
    container.replaceChildren(el("form", {onsubmit: async (e) => {
      e.preventDefault();
      save.disabled = mode.disabled = true;
      error.hidden = true;
      try {
        mode.value = (await api("settings", "PUT", {routing_mode: mode.value})).routing_mode;
        notice("Routing mode saved.");
      } catch (err) {
        error.textContent = err.message;
        error.hidden = false;
      } finally { save.disabled = mode.disabled = false; }
    }}, field("Routing mode", mode), error, save));
  } catch (err) {
    container.replaceChildren(el("p", {class: "error", role: "alert"}, err.message),
      btn("Retry", () => loadRouting(container), "refresh-cw"));
  }
}
function binaryVersions() {
  const list = el("dl", {class: "facts"});
  const status = el("p", {class: "caption", role: "status"});
  const refresh = refreshControl("Refresh binary versions", async (force = true) => {
    if (!list.childElementCount) status.textContent = "Checking versions…";
    try {
      const data = force ? await api("versions", "POST", {}) : await api("versions");
      list.replaceChildren(...Object.entries(names).flatMap(([tool, label]) => {
        const b = data[tool];
        return datum(label, b?.version ? code(b.version + (b.supported ? "" : " (unrecognized)")) : text("Unavailable"));
      }));
      status.textContent = "";
    } catch {
      status.textContent = "Could not check binary versions.";
    }
  });
  refresh.run(false);
  return el("section", {"aria-labelledby": "versions-heading"},
    el("div", {class: "sectionhead"}, el("h2", {id: "versions-heading"}, "Binary versions"), refresh.button), list, status);
}
async function settings(ver) {
  const routing = el("div");
  const content = el("div", {class: "settings"},
    el("section", {}, el("h2", {}, "New chats"), routing),
    binaryVersions(),
    el("section", {}, el("h2", {}, "Panel password"), changePasswordForm()));
  await loadRouting(routing);
  if (ver === version) main.replaceChildren(content);
}
function empty(heading, description, action) {
  return el(
    "div",
    { class: "empty" },
    el("h2", {}, heading),
    description && el("p", {}, description),
    action,
  );
}
async function loadProfiles(filter) {
  const tools = filter ? [filter] : Object.keys(names);
  const registries = await Promise.all(tools.map((tool) => api("accounts?tool=" + tool)));
  return registries.flatMap((r) => r.items);
}
function resumeCommands(row) {
  const buttons = el("div", {class: "row"});
  function add(label, glyph, cwd, args) {
    buttons.append(ib(label, glyph, () => {
      const cd = cwd ? "cd " + shell(cwd) + " && " : "";
      return copy(cd + "ccp " + row.tool + " " + args);
    }));
  }
  const label = "Copy resume command";
  if (row.tool !== "agy") {
    const arg = row.tool === "claude" ? "--resume " : "resume ";
    add(label, "copy", row.workspace, arg + row.native_id);
    return buttons;
  }

  const id = shell(row.native_id);
  const terminalArgs = "--conversation=" + id;
  if (row.evidence === "saved_file") {
    add(label, "copy", row.tui_workspace || row.workspace, terminalArgs);
    return buttons;
  }
  const hasTerminal = row.tui_workspace != null || row.saved_file_seen_ms != null;
  add(hasTerminal ? "Copy ACP resume command" : label, "copy", row.workspace, "acp --for-session " + id);
  if (hasTerminal) add("Copy terminal resume command", "square-terminal", row.tui_workspace, terminalArgs);
  return buttons;
}
function evidenceLabel(evidence) {
  if (evidence === "acp_listed") return "Listed by ACP";
  if (evidence === "codex_listed") return "Listed by Codex";
  return simple(evidence);
}
async function logs(view, params, ver, enter) {
  const sessions = view === "sessions",
    t = params.get("tool") || "",
    id = params.get("config_id") || "",
    result = params.get("state") || "";
  const q = new URLSearchParams(params);
  if (cursors.at(-1)) q.set("before", cursors.at(-1));
  const [page, profiles] = await Promise.all([
    api((sessions ? "sessions" : "startups") + "?" + q),
    loadProfiles(),
  ]);
  if (ver !== version) return;
  const configs = profiles.filter((c) => !t || c.tool === t);
  function filter(k, v) {
    const next = Object.fromEntries(params);
    next[k] = v;
    if (k === "tool") delete next.config_id;
    go(view, next);
  }
  const chooser = (k, caption, rows, val) =>
    el("label", {}, caption, select(rows, val, { onchange: (e) => filter(k, e.target.value) }));
  const byTool = chooser("tool", "Tool", [["", "All"], ...Object.entries(names)], t);
  const byConfig = chooser(
    "config_id",
    "Config",
    [["", "All"], ...configs.map((c) => [c.id, t ? c.label : names[c.tool] + " / " + c.label])],
    id,
  );
  const search = el("input", {
    type: "search",
    value: params.get("native_id") || "",
    placeholder: "Exact session ID",
    "aria-label": "Session ID",
    onchange: (e) => filter("native_id", e.target.value.trim()),
  });
  const filters = el(
    "div",
    { class: "toolbar" },
    byTool,
    byConfig,
    !sessions &&
      chooser("state", "Result", [["", "All"], ...Object.entries(states)], result),
    el("label", {}, icon("search"), search),
    sessions && refreshControl("Refresh", render).button,
  );
  const table = el("table", {class: "records", role: "table"}),
    head = el("tr"),
    body = el("tbody", {role: "rowgroup"});
  const headings = sessions
    ? ["Session ID", "Tool", "Bound config", "Workspace", "Evidence", "Last routed", "Resume"]
    : ["Time", "Tool", "Intent", "Config", "Decision", "Session ID", "Details"];
  for (const h of headings)
    head.append(el("th", { scope: "col" }, h));
  table.append(el("thead", {}, head), body);
  const short = (s) => s.length <= 14 ? s : s.slice(0, 8) + "…" + s.slice(-5);
  for (const row of page.items) {
    const cfg =
      row.config_id == null
        ? text("None")
        : el(
            "button",
            {
              type: "button",
              class: "aslink",
              onclick: () => go("accounts", { tool: row.tool, id: row.config_id }),
            },
            row.label,
          );
    const uid = row.native_id,
      alias = names[row.tool],
      unknownSession = !uid && row.state === "routed" &&
        ((row.tool === "agy" && row.intent === "tui") || (row.tool === "codex" && row.intent === "new"));
    const idLink = uid
      ? el(
          "button",
          {
            type: "button",
            class: "aslink uuid",
            title: uid,
            onclick: () =>
              view === "sessions" || (row.intent === "tui_resume" && !row.mapped)
                ? copy(uid).catch((e) => notice(e.message))
                : go("sessions", { tool: row.tool, native_id: uid }),
          },
          short(uid),
        )
      : text(row.state === "pending" ? "Pending" : unknownSession ? "Unknown" : "None");
    const state = states[row.state] || row.state;
    const decision = el(
      "div",
      {},
      pill(
        state,
        { denied: "red", exec_failed: "red", pending: "amber", observed: "blue", routed: "green" }[
          row.state
        ],
      ),
      el("div", { class: "caption" }, decisionLabels[row.reason] || simple(row.reason || "")),
    );
    const det = ib("Startup details", "chevron-right", async () => {
      expanded = expanded === row.seq ? null : row.seq;
      await render();
    });
    det.setAttribute("aria-expanded", String(expanded === row.seq));
    const cells = sessions
      ? [
          idLink,
          alias,
          el("div", {}, cfg, row.enabled === false && el("div", {}, pill("Disabled"))),
          el("span", {class:"wrap"}, (row.evidence === "saved_file" && row.tui_workspace) || row.workspace || "Unknown"),
          pill(evidenceLabel(row.evidence)),
          row.last_routed_ms == null ? "Not routed" : fmt(row.last_routed_ms),
          resumeCommands(row),
        ]
      : [fmt(row.created_ms), alias, simple(row.intent), cfg, decision, idLink, det];
    body.append(el("tr", {role: "row"}, ...cells.map((v, i) => el("td", {role: "cell"},
      el("span", {class: "cell-label", "aria-hidden": "true"}, headings[i]), v))));
    if (!sessions && expanded === row.seq)
      body.append(
        el(
          "tr",
          {role: "row"},
          el(
            "td",
            { colspan: 7, class: "details-cell", role: "cell" },
            el(
              "dl",
              { class: "facts" },
              datum("Request ID", code(row.request_id)),
              datum("Workspace", code(row.workspace || "Unavailable")),
              datum("Session ID", uid ? code(uid) : unknownSession ? "Unknown" : "Not recorded"),
              row.observed_ms && datum("ID observed", fmt(row.observed_ms)),
            ),
          ),
        ),
      );
  }
  let records;
  if (page.items.length) records = el("div", { class: "tablewrap" }, table);
  else if (cursors.length > 1) records = empty("No records on this page");
  else if (params.size) records = empty(sessions ? "No matching sessions" : "No matching startups",
    null, btn("Clear filters", () => go(view)));
  else records = sessions
    ? empty("No session mappings", "No assigned, observed or saved session IDs yet.")
    : empty("No startup requests", "Only launches through ccp appear here.");

  const root = sessions ? main : ((!enter && $("#startup-logs")) || openDashboard(profiles));
  root.replaceChildren(filters, records);
  if (!sessions) root.prepend(el("div", {class: "sectionhead"},
    el("h2", {id: "logs-heading"}, "Startup logs"), refreshControl("Refresh startup logs", render).button));
  const paginated = cursors.length > 1 || page.next_cursor != null;
  if (!page.items.length && !paginated) return;
  root.append(el(
    "div",
    { class: "pages" },
    text(page.items.length + (page.items.length === 1 ? " record" : " records") +
      (paginated ? " on this page" : ""), "caption"),
    paginated && el(
      "div",
      { class: "row" },
      ib(
        "Previous page",
        "chevron-left",
        async () => {
          cursors.pop();
          expanded = null;
          await render();
        },
        cursors.length === 1,
      ),
      "Page " + cursors.length,
      ib(
        "Next page",
        "chevron-right",
        async () => {
          cursors.push(page.next_cursor);
          expanded = null;
          await render();
        },
        page.next_cursor == null,
      ),
    ),
  ));
}
function editDirectory(c) {
  const home = el("input", {value: c.home, required: true, spellcheck: "false", autocomplete: "off",
    "aria-describedby": c.tool === "codex" ? "directory-note" : null});
  const fields = [field("Directory", home)];
  if (c.tool === "codex") fields.push(el("p", {id: "directory-note", class: "caption"},
    "Codex may still look for older transcripts at their previous paths."));
  modal("Edit directory", fields, "Save changes", async () => {
    if (home.value === c.home) return;
    await api("accounts/" + c.id, "PATCH", {revision: c.revision, home: home.value});
    notice("Directory updated.");
  });
  home.focus();
}
function accountForm(filter, c) {
  const provider = !c && !filter && select(Object.entries(names), "claude", {required: true});
  const label = el("input", { required: true, maxlength: 100, value: c?.label || "" }),
    priority = el("input", {type: "number", min: 0, max: 100, step: 1, required: true,
      value: c?.priority ?? 0, "aria-describedby": "priority-hint"}),
    home = el("input", { placeholder: "/home/user/profiles/work/config" });
  const source = select([["new", "Create new profile"], ["existing", "Use existing directory"]],
    "new", {"aria-label": "Profile directory"});
  const homeField = field("Existing config directory", home);
  const updateSource = () => {
    home.required = source.value === "existing";
    homeField.hidden = !home.required;
  };
  source.onchange = updateSource;
  updateSource();
  const mode = select(
    [
      ["inherit", "Inherit environment"],
      ["direct", "Direct connection"],
      ["custom", "Custom proxy"],
    ],
    c?.proxy_mode || "inherit",
  );
  const proxy = el("input", {
    type: "text",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: c?.proxy_url?.url || "http://proxy:3128",
  });
  const fields = el(
    "div",
    {},
    field("Proxy URL", proxy),
    c?.proxy_url && el("p", { id: "proxy-hint", class: "caption" }, "Leave blank to keep the saved proxy. Switch to Direct or Inherit to clear it."),
  );
  if (c?.proxy_url) proxy.setAttribute("aria-describedby", "proxy-hint");
  const toggle = () => (fields.hidden = mode.value !== "custom");
  mode.onchange = toggle;
  toggle();
  let heading = "Add profile";
  if (c) heading = "Edit config";
  else if (filter) heading = "Add " + names[filter] + " profile";
  modal(
    heading,
    [
      ...(provider ? [field("Provider", provider)] : []),
      field("Label", label),
      field("Priority", priority),
      el("p", {id: "priority-hint", class: "caption"}, "0–100. Higher values win in priority mode."),
      ...(!c ? [field("Profile directory", source), homeField] : []),
      field("Proxy mode", mode),
      fields,
    ],
    c ? "Save changes" : "Add profile",
    async () => {
      const t = c?.tool || filter || provider.value;
      const v = { label: label.value.trim(), proxy_mode: mode.value, priority: priority.valueAsNumber };
      if (mode.value === "custom" && proxy.value) v.proxy_url = proxy.value;
      const saved = c
        ? await api("accounts/" + c.id, "PATCH", { revision: c.revision, ...v })
        : await api("accounts", "POST", { tool: t, ...(source.value === "new" ? {create: true} : {home: home.value.trim()}), ...v });
      go("accounts", { tool: filter, id: saved.id });
      const proxyChanged = c && (mode.value !== c.proxy_mode || (mode.value === "custom" && proxy.value));
      if (!c) notice("Profile added. Not enabled for chats.");
      else if (t === "codex" && proxyChanged)
        notice("Config updated. A running Codex TUI daemon may keep its previous proxy until it restarts.");
      else notice("Config updated.");
    },
  );
}
function quotaText(q) {
  if (q.loading) return "Checking quota";
  if (q.stale) return "Stale";
  return quotaStates[q.status] || "Unavailable";
}
function quotaMeter(w, stale) {
  const value = 100 - w.used_pct;
  const expired = w.reset_at_ms != null && w.reset_at_ms <= Date.now();
  return el("progress", {value, max: 100, class: stale || expired ? "stale-meter" : "",
    "aria-label": w.label + " remaining", title: w.label + ": " + percent(value) + " left"});
}
function resetTime(ms) {
  if (ms == null) return text("Reset unknown", "caption");
  const mins = Math.ceil((ms - Date.now()) / 60000);
  let delta;
  if (mins <= 0) delta = "reset time passed";
  else if (mins >= 1440) delta = "in " + Math.ceil(mins / 1440) + "d";
  else if (mins >= 60) delta = "in " + Math.ceil(mins / 60) + "h";
  else delta = "in " + mins + " min";
  const d = new Date(ms);
  return Number.isNaN(d.getTime())
    ? text("Reset unknown")
    : el("time", {class: "reset-time", datetime: d.toISOString(), title: d.toLocaleString()},
        el("span", {}, fmt(ms)), " ", el("span", {class: mins > 0 ? "countdown" : ""}, "· " + delta));
}
function quotaDetails(q, tool) {
  const s = q.snapshot, details = document.createDocumentFragment();
  const status = (!q.success_ms || q.loading || q.stale || q.status !== "fresh") && [
    quotaText(q), q.stale && q.loading && "Stale",
    q.stale && q.status !== "fresh" && (quotaStates[q.status] || "Unavailable"),
  ].filter(Boolean).join(" · ");
  const meta = [
    s && tool === "agy" && "Model-pool limits",
    s?.plan,
    q.success_ms && (tool === "claude" ? "CLI checked " : "Last checked ") + fmt(q.success_ms),
    !q.loading && q.status !== "fresh" && q.retry_ms > Date.now() && "Retry after " + new Date(q.retry_ms).toLocaleTimeString(),
  ].filter(Boolean).join(" · ");
  let statusColor = "";
  if (q.loading) statusColor = "blue";
  else if (q.stale || q.status !== "not_checked") statusColor = "amber";
  details.append(el("p", {class: "caption quota-meta"}, status && pill(status, statusColor), meta));
  for (const w of s?.windows || []) {
    const remaining = 100 - w.used_pct;
    details.append(
      el(
        "div",
        { class: "quota" },
        el("strong", {}, w.label),
        el(
          "div",
          {class: "quota-value"},
          percent(remaining) + " left",
          quotaMeter(w, q.stale),
        ),
        resetTime(w.reset_at_ms),
      ),
    );
  }
  if (s && !s.windows.length) details.append(el("p", {}, "No windows reported"));
  if (s?.earned_resets != null)
    details.append(el("p", {}, "Earned resets available: " + s.earned_resets));
  if (s?.credits) {
    const credits = s.credits;
    let balance;
    if (credits.unlimited) balance = "Unlimited";
    else if (credits.balance != null) balance = credits.balance;
    else balance = credits.has_credits ? "Available" : "Not available";
    details.append(el("p", {}, "Credits: " + balance));
  }
  if (s?.spend_control) {
    const v = s.spend_control;
    let remaining;
    if (v.remaining_pct != null) remaining = percent(v.remaining_pct) + " left";
    else remaining = v.reached ? "Limit reached" : "Reported";
    details.append(
      el(
        "p",
        {},
        "Spend control: ",
        remaining,
        v.reset_at_ms != null ? " · " : "",
        v.reset_at_ms != null && resetTime(v.reset_at_ms),
      ),
    );
  }
  return details;
}

const resetErrors = {
  stale_grant: "This grant expired or is no longer available. List grants again for current details.",
  stale_config: "This profile or its sign-in changed. Reopen its grant list.",
  busy: "Another request is in progress. Try again shortly.",
  sign_in_required: "Sign in to this Codex profile first.",
  unsupported_build: "Could not verify the native Codex executable. Check Binary versions in Settings.",
  unavailable: "Grant details are unavailable right now.",
};
const resetOutcomes = {
  reset: "Grant redeemed. The usage limit was reset.",
  alreadyRedeemed: "This grant was already redeemed.",
  nothingToReset: "There was no usage limit to reset.",
  noCredit: "No earned reset was available.",
};
// Never persist grant handles. Keep an in-flight or uncertain attempt bound to its
// original profile so reopening the dialog cannot silently substitute a new grant.
const resetAttempts = new Map();
let resetDialog, readDashboardQuotas;
function checkResetProfile(profiles) {
  if (resetDialog && !profiles.some(c => c.id === resetDialog.id && c.revision === resetDialog.revision))
    resetDialog.close();
}
function openResetGrants(c) {
  if (resetDialog) return;
  const token = sessionToken, route = location.hash, base = "accounts/" + c.id;
  const heading = el("h2", {id: "reset-title"}, "Earned resets"),
    body = el("div", {"aria-live": "polite"}), footer = el("footer"),
    box = el("dialog", {class: "grant-dialog", "aria-labelledby": "reset-title", "aria-describedby":"reset-profile"},
      el("div", {class: "dialog-head"}, heading),
      el("div", {class: "fields"}, el("p", {id: "reset-profile"}, "Codex / " + c.label + " (#" + c.id + ")"), body), footer);
  const close = () => { if (box.open) box.close(); },
    cancel = btn("Close", close);
  const view = {id: c.id, revision: c.revision, close};
  const current = () => resetDialog === view && box.open && route === location.hash && token === sessionToken;
  resetDialog = view;
  box.addEventListener("close", () => {
    if (resetDialog === view) resetDialog = null;
    box.remove();
  }, {once: true});
  document.body.append(box);
  footer.append(cancel);
  box.showModal();

  function message(s, action) {
    body.replaceChildren(el("p", {}, s));
    footer.replaceChildren(cancel, ...(action ? [action] : []));
  }
  const expired = (g) => g.expires_at_ms !== null && g.expires_at_ms <= Date.now();
  const expiry = (g) => g.expires_at_ms === null ? "No expiration"
    : (expired(g) ? "Expired " : "Expires ") + new Date(g.expires_at_ms).toLocaleString();
  async function list() {
    if (!current()) return;
    message("Loading grants…");
    let data;
    try {
      data = await api(base + "/resets/list", "POST", {}, AbortSignal.timeout(30000));
      if (!Array.isArray(data.grants) || typeof data.details_available !== "boolean") throw new Error();
      if (!current()) return;
      // A long-open card can still have the old label when the daemon issues new handles.
      const registry = await api("accounts?tool=codex", "GET", undefined, AbortSignal.timeout(30000));
      if (!current()) return;
      if (!registry.items.some(row => row.id === c.id && row.revision === c.revision)) {
        close();
        notice(resetErrors.stale_config);
        await readQuotas();
        return;
      }
    } catch (err) {
      if (current()) message(Object.hasOwn(resetErrors, err.code) ? resetErrors[err.code] : "Could not list grants.",
        err.code !== "stale_config" && btn("List again", list));
      return;
    }
    if (!current()) return;
    const count = Number.isSafeInteger(data.available_count) && data.available_count >= 0 ? data.available_count : null;
    const rows = el("ol", {class: "grant-list"});
    for (const g of data.grants) {
      if (!g || typeof g.handle !== "string" || !g.handle || typeof g.title !== "string" || !g.title ||
          !(g.expires_at_ms === null || Number.isSafeInteger(g.expires_at_ms) && Number.isFinite(new Date(g.expires_at_ms).getTime()))) continue;
      const grant = {...g, number: rows.childElementCount + 1};
      const review = btn("Review", () => confirmGrant(grant));
      review.disabled = expired(grant);
      review.setAttribute("aria-label", "Review grant " + grant.number + ": " + grant.title);
      rows.append(el("li", {}, el("div", {class: "grant-row"},
        el("div", {}, el("strong", {}, grant.title), el("p", {class: "caption"}, expiry(grant))), review)));
    }
    const n = rows.childElementCount;
    body.replaceChildren(el("p", {}, count === null ? "Available count unknown." : "Earned resets available: " + count));
    if (!data.details_available || (count !== null && n < count) || (!n && count !== 0))
      body.append(el("p", {}, n ? "Only some grants are listed." : "No grant details available."));
    if (n) body.append(rows);
    else if (count === 0 && data.details_available) body.append(el("p", {}, "No available grants."));
  }
  function confirmGrant(grant, uncertain = false) {
    if (!current()) return;
    heading.textContent = uncertain ? "Unconfirmed redemption" : "Confirm redemption";
    const action = btn(uncertain ? "Retry same grant" : "Redeem this grant", () => submit(grant, uncertain), null, "primary");
    message(uncertain ? "The earlier attempt may have used this grant. Retrying can redeem it if it was not used."
      : "Use this one earned reset for the Codex profile above?", action);
    body.append(el("strong", {}, "Grant " + grant.number + ": " + grant.title), el("p", {class: "caption"}, expiry(grant)));
    action.disabled = !uncertain && expired(grant);
    cancel.focus();
  }
  async function readQuotas() {
    if (token !== sessionToken) return false;
    try {
      const {view} = locationState();
      if (view === "accounts") await render();
      if (view === "dashboard") await (readDashboardQuotas || render)();
      return true;
    } catch {
      return false;
    }
  }
  async function observe(attempt) {
    if (!attempt.pending) return confirmGrant(attempt.grant, true);
    message("Waiting for Codex…");
    const result = await attempt.pending;
    if (!current()) return;
    if (result.unknown) confirmGrant(attempt.grant, true);
    else {
      heading.textContent = "Earned resets";
      message(result.message);
      cancel.focus();
      if (result.refresh && !(await result.refresh) && current())
        body.append(el("p", {class: "caption"}, "Could not refresh quota. Use Refresh quota to check again."));
    }
  }
  function submit(grant, uncertain) {
    if (!current()) return;
    const prior = resetAttempts.get(c.id);
    if (prior?.pending || (prior && prior.grant !== grant)) return;
    if (!uncertain && expired(grant)) return message(resetErrors.stale_grant);
    const attempt = {revision: c.revision, grant};
    resetAttempts.set(c.id, attempt);
    async function redeem() {
      let outcome;
      try {
        ({outcome} = await api(base + "/resets/redeem", "POST", {handle: grant.handle}, AbortSignal.timeout(30000)));
      } catch (err) {
        if (uncertain && (err.code === "stale_grant" || err.code === "stale_config"))
          return {message: "The original grant can no longer be retried. The earlier redemption is unconfirmed. Check this profile in Codex."};
        if (!uncertain) {
          if (err.code === "native_rpc")
            return {message: "Redemption was not submitted. Codex could not check this grant."};
          if (err.code === "rate_limited")
            return {message: "Redemption was not submitted. The grant check was rate limited. Try again later."};
          if (Object.hasOwn(resetErrors, err.code)) return {message: resetErrors[err.code]};
        }
      }
      if (typeof outcome !== "string" || !Object.hasOwn(resetOutcomes, outcome)) return {unknown: true};
      // The daemon queues reconciliation; the visible view polls it without another POST.
      return {message: resetOutcomes[outcome], refresh: readQuotas()};
    }
    attempt.pending = redeem().then(result => {
      if (resetAttempts.get(c.id) === attempt) {
        if (result.unknown) attempt.pending = null;
        else resetAttempts.delete(c.id);
      }
      return result;
    });
    observe(attempt);
  }
  const attempt = resetAttempts.get(c.id);
  if (!attempt) list();
  else if (attempt.revision === c.revision) observe(attempt);
  else {
    const changed = "This profile changed after the previous attempt. Check the original account in Codex before listing new grants.";
    message(changed);
    Promise.resolve(attempt.pending).then(() => {
      if (current()) message(changed, btn("List current grants", () => {
        if (!current()) return;
        if (resetAttempts.get(c.id) === attempt) resetAttempts.delete(c.id);
        list();
      }));
    });
  }
}
function quotaCard(c, {showProfile = false, refreshed = render} = {}) {
  const route = location.hash;
  const accountName = showProfile && el("span", {class: "quota-name"});
  const link = showProfile && el("a", {class: "quota-link", href: "#accounts?tool=" + c.tool + "&id=" + c.id},
    text(names[c.tool], "provider-name"), accountName);
  const badge = showProfile && pill("Disabled");
  let displayedQuota;
  const refresh = refreshControl("Refresh quota", async () => {
    await api("accounts/" + c.id + "/quota/refresh", "POST", {});
    await refreshed();
  });
  const body = el("div");
  const section = el("section", {class: "quota-card"}, el("div", {class: "sectionhead"},
    el("h3", {}, showProfile ? [link, badge] : "Quota & resets"), refresh.button), body,
    c.tool === "codex" && btn("List reset grants", () => {
      if (section.isConnected && (showProfile ? locationState().view === "dashboard" : location.hash === route))
        openResetGrants(c);
    }, "layers"));
  function update(profile) {
    c = profile;
    if (link) {
      if (accountName.textContent !== profile.label) {
        accountName.textContent = profile.label;
        link.setAttribute("aria-label", names[profile.tool] + " / " + profile.label);
      }
      badge.hidden = profile.enabled;
    }
    const q = profile.quota;
    refresh.update(!!q.loading);
    const reading = q.loading && q.snapshot ? {...q, loading: false} : q;
    // Keep the previous reading (including a text selection) intact during background polls.
    const signature = JSON.stringify(reading);
    if (q.loading && signature === displayedQuota) return;
    body.replaceChildren(quotaDetails(reading, profile.tool));
    displayedQuota = signature;
  }
  update(c);
  return {section, update};
}
function openDashboard(profiles) {
  const grid = el("div", {class: "quota-grid"});
  const cards = new Map();
  const empty = el("p", {}, "No profiles yet. ", el("a", {href: "#accounts"}, "Add a profile"));
  const all = refreshControl("Refresh enabled quotas", async (staleOnly = false) => {
    await api("quota/refresh", "POST", {stale_only: staleOnly});
    await read();
  });
  const overview = el("section", {"aria-labelledby": "quota-heading"},
    el("div", {class: "sectionhead"}, el("h2", {id: "quota-heading"}, "Account quotas"), all.button), grid, empty);
  const log = el("section", {id: "startup-logs", "aria-labelledby": "logs-heading"});
  main.replaceChildren(overview, log);

  // Quota polling replaces only the meters, never a log filter being edited.
  let seq = 0, poll;
  async function read() {
    if (!overview.isConnected) return;
    clearTimeout(poll);
    const current = ++seq;
    const rows = await loadProfiles();
    if (current === seq && overview.isConnected) draw(rows);
  }
  function draw(rows) {
    checkResetProfile(rows);
    all.update(rows.some((c) => c.enabled && c.quota.loading), !rows.some((c) => c.enabled));
    empty.hidden = rows.length > 0;
    const ids = new Set(rows.map((c) => c.id));
    for (const [id, card] of cards) {
      if (!ids.has(id)) {
        card.section.remove();
        cards.delete(id);
      }
    }
    rows.forEach((c, i) => {
      const card = cards.get(c.id);
      if (card) card.update(c);
      else {
        const added = quotaCard(c, {showProfile: true, refreshed: read});
        cards.set(c.id, added);
        grid.insertBefore(added.section, grid.children[i] || null);
      }
    });
    if (rows.some((c) => c.quota.loading)) poll = setTimeout(guard(read), 1000);
  }
  readDashboardQuotas = read;
  draw(profiles);
  if (profiles.some((c) => c.enabled)) guard(() => all.run(true))();
  return log;
}
// A poll or full Accounts refresh may overlap a cancel/retry. Only use a
// status read taken after the latest action in this tab has settled.
let loginWrite = Promise.resolve();
function changeLogin(path, payload) {
  const result = loginWrite.then(() => api(path, "POST", payload));
  loginWrite = result.then(() => {}, () => {});
  return result;
}
async function readLogin(path) {
  for (;;) {
    const write = loginWrite;
    await write;
    if (write !== loginWrite) continue;
    try {
      const value = await api(path);
      if (write === loginWrite) return value;
    } catch (err) {
      if (write === loginWrite) throw err;
    }
  }
}
let selectedIdentity;
function identityView(c, login) {
  if (selectedIdentity?.id === c.id && selectedIdentity.revision === c.revision) return selectedIdentity;
  const summary = el("span", {class: "wrap", role: "status", "aria-label": "Sign-in status"});
  const button = ib("Refresh sign-in status", "refresh-cw", () => refresh(c.tool === "agy"));
  const view = {id: c.id, tool: c.tool, revision: c.revision, node: el("div", {class: "row"}, summary, button),
    completed: login?.state === "completed" ? login.attempt : null, refresh};
  selectedIdentity = view;
  let seq = 0;
  async function refresh(verifyCatalog = false) {
    const attempt = ++seq;
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
    if (verifyCatalog) view.catalogChecked = true;
    if (!summary.hasChildNodes()) summary.textContent = verifyCatalog ? "Checking catalog access…" : "Checking sign-in…";
    try {
      const endpoint = "accounts/" + c.id + "/identity";
      const v = verifyCatalog ? await api(endpoint, "POST", {}) : await api(endpoint);
      if (selectedIdentity !== view || attempt !== seq) return;
      const signedIn = (c.tool === "claude" || c.tool === "codex") && v.state === "signed_in";
      if (signedIn || (c.tool === "agy" && verifyCatalog && v.state === "catalog_accessible")) {
        const label = el("strong", {}, signedIn ? "Signed in" : "Catalog accessible");
        summary.replaceChildren(el("span", {class: "sign-in-confirmed"}, icon("check"),
          el("span", {}, label, signedIn && v.email ? ": " + v.email : "")));
      } else if (verifyCatalog) {
        summary.textContent = v.state === "sign_in_required" ? "Sign-in required" : "Couldn’t verify catalog access";
      } else {
        const labels = c.tool === "agy"
          ? {saved_login: "Saved login found (validity not checked)", no_saved_login: "No saved login"}
          : {signed_out: "Not signed in"};
        summary.textContent = labels[v.state] || "Sign-in status unavailable";
      }
    } catch (err) {
      if (attempt === seq) summary.textContent = err.code === "busy"
        ? "Other sign-in checks are running. Refresh to try again." :
          verifyCatalog ? "Couldn’t verify catalog access" : "Sign-in status unavailable";
    } finally {
      if (attempt === seq) {
        button.disabled = false;
        button.setAttribute("aria-busy", "false");
      }
    }
  }
  refresh();
  return view;
}
window.addEventListener("focus", () => {
  const view = selectedIdentity;
  if (view?.tool === "agy" && view.node.isConnected && !view.catalogChecked) view.refresh();
});
function signInPanel(c, initial) {
  const identity = identityView(c, initial);
  const section = el("section", {}, el("h3", {}, "Sign-in status"), identity.node);
  const cmd = "ccp login " + c.id;
  const terminal = el("div", {class: "row"}, code(cmd),
    ib("Copy " + names[c.tool] + " sign-in command", "copy", () => copy(cmd)));
  if (c.tool !== "codex") {
    section.append(terminal);
    return section;
  }
  const endpoint = "accounts/" + c.id + "/device-login";
  const body = el("div");
  section.append(body, el("p", {}, "Or sign in from your terminal:"), terminal);
  let last, poll;
  const update = (v) => {
    clearTimeout(poll);
    const busy = ["starting", "waiting", "cancelling"].includes(v.state);
    const refresh = async () => {
      if (!section.isConnected) return;
      try {
        const fresh = await readLogin(endpoint);
        if (section.isConnected) update(fresh);
      } catch (err) {
        if (section.isConnected) {
          last = null;
          body.replaceChildren(text(err.message), btn("Refresh sign-in", refresh, "refresh-cw"));
        }
      }
    };
    if (busy) poll = setTimeout(refresh, 1200);
    const key = JSON.stringify(v);
    if (key === last) return;
    last = key;
    const action = (label, path, payload, glyph) => btn(label, async (e) => {
      const button = e.currentTarget;
      button.disabled = true;
      clearTimeout(poll);
      try {
        const next = await changeLogin(path, payload);
        if (section.isConnected) update(next);
      } catch (err) {
        button.disabled = false;
        if (busy) await refresh();
        throw err;
      }
    }, glyph);
    if (busy) {
      const cancel = action("Cancel sign-in", endpoint + "/cancel", {attempt: v.attempt}, "x");
      cancel.disabled = v.state === "cancelling";
      body.replaceChildren(v.state === "waiting"
        ? el("div", {}, el("p", {}, "Choose the ChatGPT account for " + c.label + " and enter this code."),
            el("div", {class: "row"}, code(v.code), ib("Copy device code", "copy", () => copy(v.code)),
              el("a", {href: v.url, target: "_blank", rel: "noopener noreferrer"}, "Open OpenAI verification")))
        : el("p", {}, v.state === "starting" ? "Requesting a device code…" : "Cancelling sign-in…"),
        cancel);
    } else {
      const start = action("Sign in with ChatGPT", endpoint, {revision: c.revision}, "key-round");
      start.disabled = c.readiness !== "ready";
      const messages = {
        completed: "Device sign-in completed for " + c.label + ".",
        expired: "Device code expired. Start again for a new code.",
        failed: "Device sign-in did not complete. Check that device login is allowed for your ChatGPT account or workspace, then try again.",
        cancelled: "Stopped waiting for sign-in.",
      };
      if (v.state === "completed" && v.attempt !== identity.completed) {
        identity.completed = v.attempt;
        identity.refresh();
      }
      body.replaceChildren(...(messages[v.state] ? [el("p", {}, messages[v.state])] : []), start);
    }
  };
  update(initial);
  return section;
}
let accountQuota, accountRefresh, lastProvider;
async function accounts(params, ver, enter) {
  const filter = Object.hasOwn(names, params.get("tool")) ? params.get("tool") : "";
  const tabs = el(
    "div",
    { class: "segments", role: "group", "aria-label": "Provider" },
    [["", "All"], ...Object.entries(names)].map(([k, l]) =>
      el(
        "button",
        {
          type: "button",
          "aria-pressed": String(k === filter),
          onclick: () => go("accounts", { tool: k }),
        },
        l,
      ),
    ),
  );
  if (!enter && filter !== lastProvider) {
    showLoading("Loading " + (names[filter] || "all") + " accounts", el("div", {class: "account-toolbar"}, tabs));
  }
  lastProvider = filter;
  const items = await loadProfiles(filter);
  if (ver !== version) return;
  const c = items.find((c) => String(c.id) === params.get("id")) || items[0];
  checkResetProfile(c ? [c] : []);
  if (!c) selectedIdentity = null;
  const login = c?.tool === "codex" ? await readLogin("accounts/" + c.id + "/device-login") : null;
  if (ver !== version) return;
  const list = el("div", { class: "account-list", "aria-label": "Registered configs" });
  for (const a of items) {
    const q = a.quota;
    const windows = (q.snapshot?.windows || [])
      .filter((w) => w.kind === (a.tool === "agy" ? "model" : "account") && Number.isFinite(w.used_pct)).slice(0, 2);
    const summary = windows.map((w) => w.label + " " + percent(100 - w.used_pct) + " left")
      .join(" · ");
    list.append(
      el(
        "button",
        {
          type: "button",
          class: "account-row",
          "aria-pressed": String(a.id === c.id),
          onclick: () => go("accounts", { tool: filter, id: a.id }),
        },
        !filter && text(names[a.tool], "provider-name"),
        el(
          "span",
          { class: "row" },
          el("strong", {}, a.label),
          a.current && pill("Default", "blue"),
          pill(a.enabled ? "Enabled" : "Disabled", a.enabled ? "green" : ""),
        ),
        windows.length > 0 && el("span", {class: "quota-bars"}, windows.map((w) => quotaMeter(w, q.stale))),
        text(
          summary
            ? (q.stale ? "Stale · " : "") + summary
            : !q.snapshot
              ? quotaText(q)
              : "No quota windows",
          "caption",
        ),
      ),
    );
  }
  const detail = el("div", { class: "detail" });
  if (c) {
    if (accountQuota?.id === c.id && accountQuota.route === location.hash) {
      accountQuota.card.update(c);
    } else {
      accountQuota = {id: c.id, route: location.hash, card: quotaCard(c)};
    }
    const enabled = el("input", { type: "checkbox", checked: c.enabled, "aria-describedby": "routing-note" });
    enabled.onchange = guard(async () => {
      const next = enabled.checked;
      if (
        !next &&
        c.current &&
        !confirm(
          "Disable " + c.label + " and clear it as the default account? Maintenance commands will need a new default.",
        )
      ) {
        enabled.checked = c.enabled;
        return;
      }
      enabled.disabled = true;
      try {
        await api("accounts/" + c.id, "PATCH", { revision: c.revision, enabled: next });
        await render();
      } catch (error) {
        enabled.checked = c.enabled;
        enabled.disabled = false;
        throw error;
      }
    });
    const setDefault = btn("Set as default", async () => {
      await api("current/" + c.tool, "PUT", { config_id: c.id });
      await render();
    });
    setDefault.disabled = !c.enabled;
    const defaultAccountControl = c.current
      ? el(
          "div",
          { class: "row" },
          pill("Default", "blue"),
          ib("Clear default account", "x", async () => {
            await api("current/" + c.tool, "PUT", { config_id: null });
            await render();
          }),
        )
      : setDefault;
    detail.append(
      el(
        "div",
        { class: "detail-heading" },
        el("div", {}, !filter && text(names[c.tool], "provider-name"), el("h2", {}, c.label)),
        el(
          "div",
          { class: "actions" },
          ib("Edit config", "pencil", () => accountForm(filter, c)),
        ),
      ),
      signInPanel(c, login),
    );
    detail.append(el("section", {}, el("h3", {}, "Chat routing"),
      el("label", {class: "switch"}, enabled, "Enable profile"),
      el("p", {id: "routing-note", class: "caption"}, "Disabling this profile blocks new chats and resumes. Chats already running are not stopped."),
      el("dl", {class: "facts"},
        datum("Routing checks", text(ready[c.readiness] || c.readiness, c.readiness === "ready" ? "" : "warn")),
        datum("Priority", c.priority ?? 0),
        datum("Maintenance commands", defaultAccountControl),
        datum("Directory", el("div", {class: "actions"}, code(c.home),
          el("div", {class: "row"}, ib("Copy directory", "copy", () => copy(c.home)),
            ib("Edit directory", "pencil", () => editDirectory(c))))),
        datum("Proxy", c.proxy_mode === "custom"
          ? c.proxy_url?.url
          : simple(c.proxy_mode)))), accountQuota.card.section);
  }
  accountRefresh ??= refreshControl("Refresh enabled quotas", async () => {
    await api("quota/refresh", "POST", {});
    await render();
  });
  accountRefresh.update(items.some((a) => a.enabled && a.quota.loading), false);
  main.replaceChildren(
    el("div", {class: "account-toolbar"}, tabs, el("div", {class: "actions"},
      accountRefresh.button,
      btn("Add profile", () => accountForm(filter, null), "plus", "primary"),
    )),
    c
      ? el("div", { class: "account-grid" }, list, detail)
      : empty(
          filter ? "No " + names[filter] + " profiles" : "No profiles yet",
          "Create a profile or use an existing directory.",
        ),
  );
  if (enter) {
    await api("quota/refresh", "POST", { stale_only: true });
    if (ver === version) timer = setTimeout(() => render().catch((e) => notice(e.message)), 400);
  } else if (items.some((a) => a.quota.loading))
    timer = setTimeout(() => render().catch((e) => notice(e.message)), 1000);
}
async function render() {
  if (!sessionToken) {
    passwordForm();
    return;
  }
  clearTimeout(timer);
  const ver = ++version,
    { view, params } = locationState(),
    enter = lastView !== view;
  if (view !== "accounts") {
    selectedIdentity = null;
    accountQuota = null;
    accountRefresh = null;
  }
  if (view !== "dashboard") readDashboardQuotas = null;
  lastView = view;
  $("nav").hidden = false;
  $(".logout").hidden = false;
  for (const a of document.querySelectorAll("nav a"))
    a.getAttribute("href") === "#" + view
      ? a.setAttribute("aria-current", "page")
      : a.removeAttribute("aria-current");
  const viewName = view[0].toUpperCase() + view.slice(1);
  main.setAttribute("aria-label", viewName);
  document.title = viewName + " | cc-profile";
  if (enter) showLoading("Loading " + viewName);
  try {
    if (view === "accounts") await accounts(params, ver, enter);
    else if (view === "settings") await settings(ver);
    else await logs(view, params, ver, enter);
  } catch (err) {
    if (ver !== version || !main.contains(loadingIndicator)) throw err;
    lastView = "";
    main.replaceChildren(empty("Could not load " + viewName, err.message, btn("Retry", render, "refresh-cw")));
  }
}
window.onhashchange = guard(async () => {
  resetDialog?.close();
  cursors = [null];
  expanded = null;
  await render();
});
function showLoading(label, ...controls) {
  loadingIndicator.setAttribute("aria-label", label);
  main.replaceChildren(...controls, loadingIndicator);
}
async function connect() {
  showLoading("Connecting to router");
  let session;
  try {
    session = await api("session");
  } catch (err) {
    main.replaceChildren(empty("Could not load the panel", err.message,
      btn("Retry", connect, "refresh-cw")));
    return;
  }
  if (!session.signed_in) {
    sessionToken = "";
    sessionStorage.removeItem("ccp_session");
  }
  needsSetup = session.setup_required;
  await render();
}
await connect().catch((err) => notice(err.message));
