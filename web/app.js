const $ = (s, root = document) => root.querySelector(s);
const main = $("#main"),
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
  unsupported_build: "Native build unavailable or unsupported",
};
const quotaStates = {
  native_report: "Native quota report",
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
async function api(path, method = "GET", body) {
  const token = sessionToken;
  const opts = {
    method,
    credentials: "omit",
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
const title = (s, ...actions) =>
  el("div", { class: "titleline" }, el("h1", {}, s), el("div", { class: "actions" }, actions));
const field = (label, control) => el("label", { class: "field" }, label, control);
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
  await navigator.clipboard.writeText(s);
  notice("Copied to clipboard.");
};
const passwordInput = (name, autocomplete, props = {}) => el("input", {
  type: "password", name, required: true, maxlength: 1024, autocomplete, spellcheck: "false", ...props,
});
function passwordForm() {
  selectedIdentity = null;
  clearTimeout(timer);
  version++;
  $("nav").hidden = true;
  $(".logout").hidden = true;
  if (dialog.open) dialog.close();
  const setup = needsSetup, heading = setup ? "Set panel password" : "Sign in";
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
    const mode = select([
      ["round_robin", "Round robin"], ["most_remaining", "Most quota remaining"],
      ["least_remaining", "Least quota remaining"], ["priority", "Highest priority"],
    ], settings.routing_mode, {required: true});
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
function settings() {
  const routing = el("div");
  main.replaceChildren(title("Settings"), el("div", {class: "settings"},
    el("section", {}, el("h2", {}, "New chats"), routing),
    el("section", {}, el("h2", {}, "Panel password"), changePasswordForm())));
  return loadRouting(routing);
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
async function loadProfiles() {
  const registries = await Promise.all(Object.keys(names).map((tool) => api("accounts?tool=" + tool)));
  return registries.flatMap((r) => r.items);
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
  );
  const table = el("table"),
    head = el("tr"),
    body = el("tbody");
  for (const h of sessions
    ? ["Session ID", "Tool", "Bound config", "Workspace", "Evidence", "Last routed", "Resume"]
    : ["Time", "Tool", "Intent", "Config", "Decision", "Session ID", "Details"])
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
      alias = names[row.tool];
    const idLink = uid
      ? el(
          "button",
          {
            type: "button",
            class: "aslink uuid",
            title: uid,
            onclick: () =>
              view === "sessions" || row.intent === "tui_resume"
                ? copy(uid).catch((e) => notice(e.message))
                : go("sessions", { tool: row.tool, native_id: uid }),
          },
          short(uid),
        )
      : text(row.state === "pending" ? "Pending" : "None");
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
      el("div", { class: "caption" }, simple(row.reason || "")),
    );
    const commands = sessions
      ? ib("Copy resume command", "copy", () =>
          copy("cd " + shell(row.workspace) + " && ccp " + row.tool +
            (row.tool === "agy" ? " acp --for-session " + shell(uid) : (row.tool === "claude" ? " --resume " : " resume ") + uid)),
        )
      : null;
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
          el("span", {class:"wrap"}, row.workspace),
          pill(simple(row.evidence)),
          fmt(row.last_routed_ms),
          commands,
        ]
      : [fmt(row.created_ms), alias, simple(row.intent), cfg, decision, idLink, det];
    body.append(el("tr", {}, ...cells.map((v) => el("td", {}, v))));
    if (!sessions && expanded === row.seq)
      body.append(
        el(
          "tr",
          {},
          el(
            "td",
            { colspan: 7, class: "details-cell" },
            el(
              "dl",
              { class: "facts" },
              datum("Request ID", code(row.request_id)),
              datum("Workspace", code(row.workspace || "Unavailable")),
              datum("Session ID", uid ? code(uid) : "Not recorded"),
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
    ? empty("No session mappings", "Session IDs appear here when assigned or observed.")
    : empty("No startup requests", "Only launches through ccp appear here.");

  const root = sessions ? main : ((!enter && $("#startup-logs")) || openDashboard(profiles));
  root.replaceChildren(sessions ? title("Sessions", ib("Refresh", "refresh-cw", render)) :
    el("div", {class: "sectionhead"}, el("h2", {id: "logs-heading"}, "Startup logs"),
      ib("Refresh startup logs", "refresh-cw", render)), filters, records);
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
function accountForm(t, c) {
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
  modal(
    c ? "Edit config" : "Add " + names[t] + " profile",
    [
      field("Label", label),
      field("Priority", priority),
      el("p", {id: "priority-hint", class: "caption"}, "0–100. Higher values win in priority mode."),
      ...(c ? [field("Config directory", code(c.home))] : [field("Profile directory", source), homeField]),
      field("Proxy mode", mode),
      fields,
    ],
    c ? "Save changes" : "Add profile",
    async () => {
      const v = { label: label.value.trim(), proxy_mode: mode.value, priority: priority.valueAsNumber };
      if (mode.value === "custom" && proxy.value) v.proxy_url = proxy.value;
      const saved = c
        ? await api("accounts/" + c.id, "PATCH", { revision: c.revision, ...v })
        : await api("accounts", "POST", { tool: t, ...(source.value === "new" ? {create: true} : {home: home.value.trim()}), ...v });
      go("accounts", { tool: t, id: saved.id });
      notice(c ? "Config updated." : "Profile added. Not enabled for chats.");
    },
  );
}
const quotaText = (q) =>
  q.loading ? "Checking quota" : q.stale ? "Stale" : quotaStates[q.status] || "Unavailable";
function resetTime(ms) {
  if (ms == null) return text("Reset unknown", "caption");
  const mins = Math.ceil((ms - Date.now()) / 60000);
  const delta =
    mins <= 0
      ? "reset time passed"
      : mins >= 1440
        ? "in " + Math.ceil(mins / 1440) + "d"
        : mins >= 60
          ? "in " + Math.ceil(mins / 60) + "h"
          : "in " + mins + " min";
  const d = new Date(ms);
  return Number.isNaN(d.getTime())
    ? text("Reset unknown")
    : el("time", {class: "reset-time", datetime: d.toISOString(), title: d.toLocaleString()},
        el("span", {}, fmt(ms)), " ", el("span", {class: mins > 0 ? "countdown" : ""}, "· " + delta));
}
function quotaDetails(q) {
  const s = q.snapshot, details = document.createDocumentFragment();
  details.append(el("p", {class: "caption"}, [
    (!q.success_ms || q.loading || q.stale || q.status !== "fresh") && quotaText(q),
    q.stale && q.status !== "fresh" && (quotaStates[q.status] || "Unavailable"),
    s?.plan,
    q.success_ms && "Last checked " + fmt(q.success_ms),
  ].filter(Boolean).join(" · ")));
  for (const w of s?.windows || []) {
    const remaining = 100 - w.used_pct;
    details.append(
      el(
        "div",
        { class: "quota" },
        el("strong", {}, w.label),
        el(
          "div",
          {},
          percent(remaining) + " left",
          el("progress", { value: remaining, max: 100, "aria-label": w.label + " remaining" }),
        ),
        resetTime(w.reset_at_ms),
      ),
    );
  }
  if (s && !s.windows.length) details.append(el("p", {}, "No windows reported"));
  if (s?.credits)
    details.append(
      el(
        "p",
        {},
        "Credits: " +
          (s.credits.unlimited
            ? "Unlimited"
            : (s.credits.balance ?? (s.credits.has_credits ? "Available" : "Not available"))),
      ),
    );
  if (s?.spend_control) {
    const v = s.spend_control;
    details.append(
      el(
        "p",
        {},
        "Spend control: ",
        v.remaining_pct != null
          ? percent(v.remaining_pct) + " left"
          : v.reached
            ? "Limit reached"
            : "Reported",
        v.reset_at_ms != null ? " · " : "",
        v.reset_at_ms != null && resetTime(v.reset_at_ms),
      ),
    );
  }
  return details;
}
function quotaCard(c, {showProfile = false, refreshed = render} = {}) {
  const link = showProfile && el("a", {href: "#accounts?tool=" + c.tool + "&id=" + c.id});
  const badge = showProfile && pill("Disabled");
  let submitting = false, loading = false;
  // Keep keyboard focus while either the request or the daemon's quota check is pending.
  const syncButton = () => button.setAttribute("aria-disabled", String(submitting || loading));
  const native = c.tool === "agy";
  const button = ib(native ? "View native quota report" : "Refresh quota", native ? "activity" : "refresh-cw", async () => {
    if (submitting || loading) return;
    submitting = true;
    syncButton();
    try {
      if (native) {
        const result = await api("accounts/" + c.id + "/quota/report", "POST", {});
        if (section.isConnected && !dialog.open)
          modal("Quota: " + c.label, [el("pre", {class: "wrap"}, result.report)], "Close");
        return;
      }
      await api("accounts/" + c.id + "/quota/refresh", "POST", {});
      await refreshed();
    } finally {
      submitting = false;
      syncButton();
    }
  });
  const body = el("div");
  if (native) {
    const cmd = "ccp usage " + c.id;
    body.append(el("div", {class: "row"}, code(cmd), ib("Copy quota command", "copy", () => copy(cmd))));
  }
  const section = el("section", {class: "quota-card"}, el("div", {class: "sectionhead"},
    el("h3", {}, showProfile ? [link, badge] : "Quota & resets"), button), body);
  function update(profile) {
    c = profile;
    if (link) {
      const label = names[profile.tool] + " / " + profile.label;
      if (link.textContent !== label) link.textContent = label;
      badge.hidden = profile.enabled;
    }
    loading = !!profile.quota.loading;
    syncButton();
    if (!native) body.replaceChildren(quotaDetails(profile.quota));
  }
  update(c);
  return {section, update};
}
function openDashboard(profiles) {
  const grid = el("div", {class: "quota-grid"});
  const cards = new Map();
  const empty = el("p", {}, "No profiles yet. ", el("a", {href: "#accounts"}, "Add a profile"));
  const refresh = async (staleOnly) => {
    await api("quota/refresh", "POST", {stale_only: staleOnly});
    await read();
  };
  const all = ib("Refresh enabled quotas", "refresh-cw", () => refresh(false));
  const overview = el("section", {"aria-labelledby": "quota-heading"},
    el("div", {class: "sectionhead"}, el("h2", {id: "quota-heading"}, "Account quotas"), all), grid, empty);
  const log = el("section", {id: "startup-logs", "aria-labelledby": "logs-heading"});
  main.replaceChildren(title("Dashboard"), overview, log);

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
    all.disabled = !rows.some((c) => c.enabled && c.tool !== "agy");
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
  draw(profiles);
  if (profiles.some((c) => c.enabled && c.tool !== "agy")) guard(() => refresh(true))();
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
  const summary = el("span", {class: "wrap", role: "status"}, "Checking sign-in…");
  const button = ib("Refresh sign-in status", "refresh-cw", refresh);
  const view = {id: c.id, tool: c.tool, revision: c.revision, node: el("div", {class: "row"}, summary, button),
    completed: login?.state === "completed" ? login.attempt : null, refresh};
  selectedIdentity = view;
  let seq = 0;
  async function refresh() {
    const attempt = ++seq;
    button.disabled = true;
    try {
      const v = await api("accounts/" + c.id + "/identity");
      if (selectedIdentity !== view || attempt !== seq) return;
      const labels = {signed_in: "Signed in", signed_out: "Not signed in",
        saved_login: "Saved login found (validity not checked)", no_saved_login: "No saved login"};
      summary.textContent = (labels[v.state] || "Sign-in status unavailable") + (v.email ? ": " + v.email : "");
    } catch (err) {
      if (attempt === seq) summary.textContent = err.code === "busy"
        ? "Other sign-in checks are running. Refresh to try again." : "Sign-in status unavailable";
    } finally { if (attempt === seq) button.disabled = false; }
  }
  refresh();
  return view;
}
window.addEventListener("focus", () => {
  if (selectedIdentity?.tool === "agy" && selectedIdentity.node.isConnected) selectedIdentity.refresh();
});
function signInPanel(c, initial, supported) {
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
      start.disabled = !c.present || !supported;
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
async function accounts(params, ver, enter) {
  const t = Object.hasOwn(names, params.get("tool")) ? params.get("tool") : "claude",
    data = await api("accounts?tool=" + t);
  if (ver !== version) return;
  const c = data.items.find((c) => String(c.id) === params.get("id")) || data.items[0];
  if (!c) selectedIdentity = null;
  const login = c && t === "codex" ? await readLogin("accounts/" + c.id + "/device-login") : null;
  if (ver !== version) return;
  const tabs = el(
    "div",
    { class: "segments", "aria-label": "Provider" },
    ...Object.entries(names).map(([k, l]) =>
      el(
        "button",
        {
          type: "button",
          "aria-pressed": String(k === t),
          onclick: () => go("accounts", { tool: k }),
        },
        l,
      ),
    ),
  );
  const list = el("div", { class: "account-list", "aria-label": "Registered configs" });
  for (const a of data.items) {
    const q = a.quota;
    const windows = (q.snapshot?.windows || [])
      .filter((w) => w.kind === "account" && Number.isFinite(w.used_pct)).slice(0, 2);
    const summary = windows.map((w) => w.label + " " + percent(100 - w.used_pct) + " left")
      .join(" · ");
    list.append(
      el(
        "button",
        {
          type: "button",
          class: "account-row",
          "aria-pressed": String(a.id === c.id),
          onclick: () => go("accounts", { tool: t, id: a.id }),
        },
        el(
          "span",
          { class: "row" },
          el("strong", {}, a.label),
          a.current && pill("Current", "blue"),
          pill(a.enabled ? "Enabled" : "Disabled", a.enabled ? "green" : ""),
        ),
        windows.length > 0 && el("span", {class: "quota-bars"}, windows.map((w) =>
          el("progress", {max: 100, value: 100 - w.used_pct, "aria-label": w.label + " remaining",
            title: w.label + ": " + percent(100 - w.used_pct) + " left"}))),
        text(
          summary
            ? (q.stale ? "Stale · " : "") + summary
            : !q.snapshot
              ? quotaText(q)
              : "No account windows",
          "caption",
        ),
      ),
    );
  }
  const detail = el("div", { class: "detail" });
  if (c) {
    const enabled = el("input", { type: "checkbox", checked: c.enabled, "aria-describedby": "routing-note" });
    enabled.onchange = guard(async () => {
      const next = enabled.checked;
      if (
        !next &&
        c.current &&
        !confirm(
          "Disable " + c.label + " and clear Current? Misc commands will need a new Current.",
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
    const set = btn("Set current", async () => {
      await api("current/" + t, "PUT", { config_id: c.id });
      await render();
    });
    set.disabled = !c.enabled;
    const current = c.current
      ? el(
          "div",
          { class: "row" },
          pill("Current", "blue"),
          ib("Clear current", "x", async () => {
            await api("current/" + t, "PUT", { config_id: null });
            await render();
          }),
        )
      : set;
    detail.append(
      el(
        "div",
        { class: "detail-heading" },
        el("h2", {}, c.label),
        el(
          "div",
          { class: "actions" },
          ib("Edit config", "pencil", () => accountForm(t, c)),
        ),
      ),
      signInPanel(c, login, data.supported),
    );
    detail.append(el("section", {}, el("h3", {}, "Chat routing"),
      el("label", {class: "switch"}, enabled, "Enable profile"),
      el("p", {id: "routing-note", class: "caption"}, "Disabling this profile blocks new chats and resumes. Chats already running are not stopped."),
      el("dl", {class: "facts"},
        datum("Routing checks", text(ready[c.readiness] || c.readiness, c.readiness === "ready" ? "" : "warn")),
        datum("Priority", c.priority ?? 0),
        datum("Maintenance commands", current),
        datum("Directory", code(c.home)),
        datum("Proxy", c.proxy_mode === "custom"
          ? c.proxy_url?.url
          : simple(c.proxy_mode)))), quotaCard(c).section);
  }
  main.replaceChildren(
    title(
      "Accounts",
      ib("Refresh enabled quotas", "refresh-cw", async () => {
        await api("quota/refresh", "POST", {});
        await render();
      }),
      btn("Add profile", () => accountForm(t, null), "plus", "primary"),
    ),
    el(
      "div",
      { class: "titleline" },
      tabs,
      text(data.version || "Native tool not found", "caption"),
    ),
    c
      ? el("div", { class: "account-grid" }, list, detail)
      : empty(
          "No " + names[t] + " profiles",
          "Create a profile or use an existing directory.",
        ),
  );
  if (enter && t !== "agy") {
    await api("quota/refresh", "POST", { stale_only: true });
    if (ver === version) timer = setTimeout(() => render().catch((e) => notice(e.message)), 400);
  } else if (data.items.some((a) => a.quota.loading))
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
  if (view !== "accounts") selectedIdentity = null;
  lastView = view;
  $("nav").hidden = false;
  $(".logout").hidden = false;
  for (const a of document.querySelectorAll("nav a"))
    a.getAttribute("href") === "#" + view
      ? a.setAttribute("aria-current", "page")
      : a.removeAttribute("aria-current");
  document.title = view[0].toUpperCase() + view.slice(1) + " | cc-profile";
  if (view === "accounts") await accounts(params, ver, enter);
  else if (view === "settings") await settings();
  else await logs(view, params, ver, enter);
}
window.onhashchange = guard(async () => {
  cursors = [null];
  expanded = null;
  await render();
});
async function connect() {
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
