const $ = (s, root = document) => root.querySelector(s);
const main = $("#main"),
  dialog = $("#dialog"),
  form = $("#form");
const names = { claude: "Claude", codex: "Codex" };
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
  observer_required: "Observer approval needed",
  path_missing: "Directory missing or replaced",
  unsupported_build: "Native build unavailable or unsupported",
};
const quotaStates = {
  not_checked: "Not checked",
  sign_in_required: "Sign-in required",
  rate_limited: "Rate limited",
  unavailable: "Unavailable",
  fresh: "Checked",
};
let csrf = "",
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
  const opts = {
    method,
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
  };
  if (body !== undefined) opts.body = JSON.stringify(body);
  const r = await fetch("/api/v1/" + path, opts);
  const v = await r.json().catch(() => ({}));
  if (!r.ok) {
    if (r.status === 403 && csrf) {
      csrf = "";
      passwordForm();
    }
    throw Object.assign(new Error(v.message || "Request failed (" + r.status + ")."), {code: v.error});
  }
  return v;
}
function locationState() {
  const [v, q] = location.hash.slice(1).split("?");
  return {
    view: ["accounts", "sessions", "startups"].includes(v) ? v : "startups",
    params: new URLSearchParams(q),
  };
}
function go(view, obj = {}) {
  const q = new URLSearchParams(Object.entries(obj).filter(([, v]) => v != null && v !== ""));
  location.hash = view + (q.size ? "?" + q : "");
}
const title = (s, ...actions) =>
  el("div", { class: "titleline" }, el("h1", {}, s), el("div", { class: "actions" }, actions));
const field = (label, control, note) =>
  el("label", { class: "field" }, label, control, note && el("small", {}, note));
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
  save.textContent = submit;
  save.disabled = false;
  err.hidden = true;
  form.onsubmit = async (e) => {
    e.preventDefault();
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
function passwordForm() {
  clearTimeout(timer);
  version++;
  $("nav").hidden = true;
  $(".logout").hidden = true;
  if (dialog.open) dialog.close();
  const setup = needsSetup, heading = setup ? "Set panel password" : "Sign in";
  document.title = heading + " | cc-profile";
  const input = (name) => el("input", {type: "password", name, required: true, maxlength: 1024,
    autocomplete: setup ? "new-password" : "current-password", spellcheck: "false"});
  const password = input("password"), confirm = setup ? input("confirmation") : null;
  const submit = el("button", {type: "submit", class: "primary"}, setup ? "Set password" : "Sign in");
  const error = el("p", { class: "error", role: "alert", hidden: true });
  const f = el(
    "form",
    { class: "password-form" },
    el("span", { class: "brandmark" }, icon("key-round")),
    el("h1", {}, heading),
    setup && el("p", { class: "muted" }, "Use at least 12 characters."),
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
      csrf = (await api(setup ? "setup" : "login", "POST", {password: password.value})).csrf;
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
$(".logout").onclick = guard(async () => {
  await api("logout", "POST", {});
  csrf = "";
  passwordForm();
});
function empty(glyph, heading, description, action) {
  return el(
    "div",
    { class: "empty" },
    icon(glyph),
    el("h2", {}, heading),
    el("p", {}, description),
    action,
  );
}
async function logs(view, params, ver) {
  const sessions = view === "sessions",
    t = params.get("tool") || "",
    id = params.get("config_id") || "",
    result = params.get("state") || "";
  const q = new URLSearchParams(params);
  if (cursors.at(-1)) q.set("before", cursors.at(-1));
  const [page, ...registries] = await Promise.all([
    api(view + "?" + q),
    ...Object.keys(names).map((t) => api("accounts?tool=" + t)),
  ]);
  if (ver !== version) return;
  const configs = registries.flatMap((r) => r.items).filter((c) => !t || c.tool === t);
  function filter(k, v) {
    const next = Object.fromEntries(params);
    next[k] = v;
    if (k === "tool") delete next.config_id;
    go(view, next);
  }
  const chooser = (k, caption, rows, val) =>
    el("label", {}, caption, select(rows, val, { onchange: (e) => filter(k, e.target.value) }));
  const byTool = chooser("tool", "Tool", [["", "All tools"], ...Object.entries(names)], t);
  const byConfig = chooser(
    "config_id",
    "Config",
    [["", "All configs"], ...configs.map((c) => [c.id, names[c.tool] + " / " + c.label])],
    id,
  );
  const search = el("input", {
    type: "search",
    value: params.get("native_id") || "",
    placeholder: "Exact session UUID",
    "aria-label": "Session UUID",
    onchange: (e) => filter("native_id", e.target.value.trim()),
  });
  const filters = el(
    "div",
    { class: "toolbar" },
    byTool,
    byConfig,
    !sessions &&
      chooser("state", "Result", [["", "All results"], ...Object.entries(states)], result),
    el("label", {}, icon("search"), search),
  );
  const table = el("table"),
    head = el("tr"),
    body = el("tbody");
  for (const h of sessions
    ? ["Session UUID", "Tool", "Bound config", "Workspace", "Evidence", "Last routed", "Resume"]
    : ["Time", "Tool", "Intent", "Config", "Decision", "Session UUID", "Details"])
    head.append(el("th", { scope: "col" }, h));
  table.append(el("thead", {}, head), body);
  const short = (s) => s.slice(0, 8) + "…" + s.slice(-5);
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
              view === "sessions"
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
            (row.tool === "claude" ? " --resume " : " resume ") + uid),
        )
      : null;
    const det = ib("Startup details", "chevron-right", async () => {
      expanded = expanded === row.seq ? null : row.seq;
      await render();
    });
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
              datum("Session UUID", uid ? code(uid) : "Not recorded"),
              datum("Decision", simple(row.reason)),
              row.observed_ms && datum("ID observed", fmt(row.observed_ms)),
            ),
          ),
        ),
      );
  }
  const tools = el("div", { class: "tablewrap" }, table);
  if (!page.items.length)
    tools.append(
      empty(
        sessions ? "git-branch" : "activity",
        sessions ? "No session mappings" : "No startup requests",
        params.size
          ? "No records match these filters."
          : sessions
            ? "Assigned and observed session IDs appear here."
            : "No managed launches recorded.",
        btn(params.size ? "Clear filters" : "Accounts", () => go(params.size ? view : "accounts")),
      ),
    );
  const pager = el(
    "div",
    { class: "pages" },
    text(page.items.length + " records on this page", "caption"),
    el(
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
  );
  main.replaceChildren(
    title(sessions ? "Sessions" : "Startups", ib("Refresh", "refresh-cw", render)),
    filters,
    tools,
    pager,
  );
}
function accountForm(t, c) {
  const label = el("input", { required: true, maxlength: 100, value: c?.label || "" }),
    home = el("input", { required: true, placeholder: "/home/user/profiles/work/config" });
  const mode = select(
    [
      ["inherit", "Inherit environment"],
      ["direct", "Direct connection"],
      ["custom", "Custom proxy"],
    ],
    c?.proxy_mode || "inherit",
  );
  const http = el("input", {
    type: "password",
    autocomplete: "new-password",
    placeholder: c?.http_proxy?.url || "http://proxy:3128",
  });
  const https = el("input", {
    type: "password",
    autocomplete: "new-password",
    placeholder: c?.https_proxy?.url || "http://proxy:3128",
  });
  const fields = el(
    "div",
    {},
    ...[
      ["HTTP_PROXY", http, c?.http_proxy],
      ["HTTPS_PROXY", https, c?.https_proxy],
    ].map(([k, input, saved]) => {
      const clear = el("input", { type: "checkbox", "data-clear": k });
      return el(
        "div",
        {},
        field(k, input, saved ? "Blank keeps the saved value." : null),
        saved && el("label", { class: "switch" }, clear, "Clear saved " + k),
      );
    }),
  );
  const toggle = () => (fields.hidden = mode.value !== "custom");
  mode.onchange = toggle;
  toggle();
  modal(
    c ? "Edit config" : "Add " + names[t] + " config",
    [
      field("Label", label),
      c ? field("Config directory", code(c.home)) : field("Existing config directory", home),
      field("Proxy mode", mode),
      fields,
    ],
    c ? "Save changes" : "Add config",
    async () => {
      const v = { label: label.value.trim(), proxy_mode: mode.value };
      if (mode.value === "custom")
        for (const [k, input] of [
          ["http_proxy", http],
          ["https_proxy", https],
        ]) {
          if (input.value) v[k] = input.value;
          else if ($('[data-clear="' + k.toUpperCase() + '"]', fields)?.checked) v[k] = null;
        }
      const saved = c
        ? await api("accounts/" + c.id, "PATCH", { revision: c.revision, ...v })
        : await api("accounts", "POST", { tool: t, home: home.value.trim(), ...v });
      go("accounts", { tool: t, id: saved.id });
      notice(c ? "Config updated." : "Config registered, initially disabled.");
    },
  );
}
function compatibility(t, compat) {
  const root = el("input", {
      value: compat?.root || "",
      placeholder: "/home/user/.config/cc-profile",
    }),
    outer = el("input", { value: compat?.outer || "", placeholder: "/home/user/." + t });
  const active = el("input", { type: "checkbox", checked: !!compat }),
    preview = el("p", { class: "wrap mono" });
  function show() {
    root.disabled = outer.disabled = !active.checked;
    root.required = outer.required = active.checked;
    preview.textContent = active.checked
      ? (outer.value || "Native path") +
        " → " +
        (root.value || "Profile root") +
        "/" +
        t +
        "/current/config"
      : "No compatibility projection.";
  }
  active.onchange = root.oninput = outer.oninput = show;
  show();
  const fields = [
    el("label", { class: "switch" }, active, "Project Current into native paths"),
    field("Existing profile root", root),
    field("Outer symlink path", outer),
    preview,
  ];
  if (compat)
    fields.push(
      el(
        "p",
        { class: "wrap mono" },
        "Observed: " + compat.current + " → " + (compat.observed_current || "not linked"),
      ),
    );
  modal("Compatibility links", fields, "Apply links", async () => {
    await api("compatibility/" + t, "PUT", {
      root: active.checked ? root.value : null,
      outer: active.checked ? outer.value : null,
    });
  });
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
    : el("time", { datetime: d.toISOString(), title: d.toLocaleString() }, fmt(ms) + " · " + delta);
}
function capacity(c) {
  const q = c.quota,
    s = q.snapshot;
  const section = el(
    "section",
    {},
    el(
      "div",
      { class: "sectionhead" },
      el("h3", {}, "Quota & resets"),
      ib(
        "Refresh quota",
        "refresh-cw",
        async () => {
          await api("accounts/" + c.id + "/quota/refresh", "POST", {});
          await render();
        },
        q.loading,
      ),
    ),
    el(
      "p",
      { class: "caption" },
      quotaText(q),
      s?.plan ? " · " + s.plan : "",
      q.success_ms ? " · Last checked " + fmt(q.success_ms) : "",
      q.stale && q.status !== "fresh" ? " · " + (quotaStates[q.status] || "Unavailable") : "",
    ),
  );
  for (const w of s?.windows || []) {
    const remaining = 100 - w.used_pct;
    section.append(
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
  if (s && !s.windows.length) section.append(el("p", {}, "No windows reported"));
  if (s?.credits)
    section.append(
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
    section.append(
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
  return section;
}
async function accounts(params, ver, enter) {
  const t = params.get("tool") === "codex" ? "codex" : "claude",
    data = await api("accounts?tool=" + t);
  if (ver !== version) return;
  const c = data.items.find((c) => String(c.id) === params.get("id")) || data.items[0];
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
    const summary = q.snapshot?.windows
      .filter((w) => w.kind === "account")
      .slice(0, 2)
      .map((w) => w.label + " " + percent(100 - w.used_pct) + " left")
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
    const enabled = el("input", { type: "checkbox", checked: c.enabled, "aria-label": "Enabled" });
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
      el(
        "dl",
        { class: "facts" },
        datum("Directory", code(c.home)),
        datum("Eligible", el("label", { class: "switch" }, enabled, "Enabled")),
        datum("Current", current),
        datum(
          "Readiness",
          text(ready[c.readiness] || c.readiness, c.readiness === "ready" ? "" : "warn"),
        ),
        datum(
          "Proxy",
          c.proxy_mode === "custom"
            ? [c.http_proxy?.url, c.https_proxy?.url].filter(Boolean).join(" · ")
            : simple(c.proxy_mode),
        ),
      ),
      capacity(c),
    );
    if (t === "codex") {
      const cmd = "CODEX_HOME=" + shell(c.home) + " codex --no-daemon";
      detail.append(
        el(
          "section",
          {},
          el("h3", {}, "Startup observer"),
          el(
            "p",
            { class: "caption" },
            "Native approval: run the scoped command, then open /hooks.",
          ),
          el(
            "div",
            { class: "row" },
            code(cmd),
            ib("Copy approval command", "copy", () => copy(cmd)),
          ),
          el(
            "div",
            { class: "actions" },
            btn("Install observer", () =>
              modal(
                "Install observer",
                [
                  el("p", { class: "wrap" }, code(c.home + "/hooks.json")),
                  el("pre", { class: "wrap" }, JSON.stringify(data.observer, null, 2)),
                ],
                "Install",
                async () => {
                  await api("accounts/" + c.id + "/observer/install", "POST", {
                    revision: c.revision,
                  });
                  notice("Observer installed. Approve it in /hooks.");
                },
              ),
            ),
            btn(
              "Check observer",
              async () => {
                await api("accounts/" + c.id + "/observer/check", "POST", { revision: c.revision });
                notice("Trusted startup observer verified.");
                await render();
              },
              "check",
            ),
          ),
        ),
      );
    }
  }
  const compat = data.compatibility;
  main.replaceChildren(
    title(
      "Accounts",
      ib("Refresh enabled quotas", "refresh-cw", async () => {
        await api("quota/refresh", "POST", {});
        await render();
      }),
      btn("Add config", () => accountForm(t, null), "plus", "primary"),
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
          "users",
          "No " + names[t] + " configs",
          "Register an existing config directory.",
          btn("Add config", () => accountForm(t, null), "plus"),
        ),
    el(
      "div",
      { class: "pages" },
      text(
        compat
          ? compat.needs_repair
            ? "Compatibility links need repair"
            : "Compatibility links configured"
          : "Compatibility links off",
        "caption",
      ),
      el(
        "div",
        { class: "row" },
        compat?.needs_repair &&
          btn("Repair", async () => {
            await api("current/" + t + "/repair", "POST", {});
            await render();
          }),
        btn("Compatibility links", () => compatibility(t, compat), "link"),
      ),
    ),
  );
  if (enter) {
    await api("quota/refresh", "POST", { stale_only: true });
    if (ver === version) timer = setTimeout(() => render().catch((e) => notice(e.message)), 400);
  } else if (data.items.some((a) => a.quota.loading))
    timer = setTimeout(() => render().catch((e) => notice(e.message)), 1000);
}
async function render() {
  if (!csrf) {
    passwordForm();
    return;
  }
  clearTimeout(timer);
  const ver = ++version,
    { view, params } = locationState(),
    enter = lastView !== view;
  lastView = view;
  $("nav").hidden = false;
  $(".logout").hidden = false;
  for (const a of document.querySelectorAll("nav a"))
    a.getAttribute("href") === "#" + view
      ? a.setAttribute("aria-current", "page")
      : a.removeAttribute("aria-current");
  document.title = view[0].toUpperCase() + view.slice(1) + " | cc-profile";
  if (view === "accounts") await accounts(params, ver, enter);
  else await logs(view, params, ver);
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
    main.replaceChildren(empty("route", "Could not load the panel", err.message,
      btn("Retry", connect, "refresh-cw")));
    return;
  }
  csrf = session.csrf || "";
  needsSetup = session.setup_required;
  await render();
}
await connect().catch((err) => notice(err.message));
