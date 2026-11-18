/*
  app.js - the whole browser side of the Orthanc Portal (vanilla JavaScript).

  The server only exposes a JSON API (see src/routes/mod.rs); this file draws
  the interface and calls it. No framework and no build step: edit, save,
  rebuild the Rust program (the file is embedded in the executable) and reload.

  Sections
    1. Settings and state
    2. Tiny DOM helper `h()`        (builds elements safely, never uses innerHTML)
    3. API helper `api()`           (adds the CSRF header, handles errors / expired sessions)
    4. Formatting helpers           (DICOM names, dates)
    5. Shell: login, navigation, sign out, colours (theme)
   5b. Simplified view               (header, icon column, home page with recent studies)
    6. Studies and Series search    (shared builder, modality picker, two tables)
   6b. Preview thumbnails           (lazy loading, small in-memory cache)
    7. Viewers (Weasis / OHIF), downloads and sending to DICOM nodes
    8. Upload                       (queue, parallel XHR uploads, drag & drop)
    9. Transfers, Users (admin) and Audit log
   9a. Sharing studies with people outside (Share dialog, Shares tab)
   9c. Help                         (instructions for the user reading them)
   9b. Settings and DICOM nodes (admin, intranet only): the whole configuration,
       including the branding (window title, website icon, logo)
   10. Start-up

  SECURITY NOTE: values coming from DICOM files (patient names, descriptions...)
  are untrusted. They are only ever inserted as text nodes by `h()`, so they can
  never inject HTML or scripts into the page.
*/
"use strict";

/* ====================================================================
   1. SETTINGS AND STATE
   ==================================================================== */

const PAGE_SIZE = 25;                 // rows per page in the search tables
const UPLOAD_CONCURRENCY = 3;         // files sent to the server at the same time

/**
 * Quick modality filters shown as toggle buttons: `{ label, codes, title }`. `codes` are
 * the DICOM modality codes sent to the server (RX = CR + DX). Administrators choose
 * which ones are shown and in what order (Settings > General > Modality buttons); the
 * server sends the visible ones, in order, with the user's features.
 */
const modalityButtons = () => (state.user && state.user.features && state.user.features.modality_buttons) || [];

/** Global UI state. `user` is null while signed out. */
const state = {
  user: null,     // { id, username, permissions: ["read", ...], features: { ohif, thumbnails, share: {...} } }
  view: null,     // id of the visible tab ("studies", "series", "upload", "settings"...)
  simpleView: "home", // id of the visible page of the simplified view (see SIMPLE_PAGES)
  simpleSearch: "studies", // simplified Search page: "studies" or "series"
  settingsTab: "general", // id of the visible tab inside Settings (see SETTINGS_TABS)
  notice: "",     // message shown on the login form (e.g. "Your session expired")
  // Identity of this installation (Settings > Branding). Filled from /api/public.
  brand: { name: "Orthanc Portal", title: "Orthanc Portal", show_name: true, login_notice: "", logo: null,
           favicon: { url: "/favicon.svg", type: "image/svg+xml", custom: false },
           theme: { mode: "auto", accent: "", header: "", sidebar: "", background: "" } },
  orthancProblem: "",     // administrators: why the Orthanc connection is not healthy ("" = fine)
  orthancCheckedAt: 0,    // when it was last checked (ms), to test at most once a minute
};

const root = document.getElementById("app");

/** Does the signed-in user own this permission? (UI convenience only: the server enforces it.) */
const can = (permission) => !!state.user && state.user.permissions.includes(permission);

/** Is the OHIF browser viewer available? (Settings, and Orthanc's plugins when "automatic") */
const hasOhif = () => !!(state.user && state.user.features && state.user.features.ohif);

/** Is the Weasis desktop viewer offered? (Settings > General > Viewers and uploads) */
const hasWeasis = () => !!(state.user && state.user.features && state.user.features.weasis);

/* ====================================================================
   2. DOM HELPER
   ==================================================================== */

/**
 * h("div", { class: "x", onclick: fn }, child1, [child2, child3], "text")
 * Creates an element. Strings become text nodes (safe), arrays are flattened,
 * null/false children are skipped. Props named onXxx add event listeners.
 */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (["value", "checked", "disabled", "hidden", "selected"].includes(key)) el[key] = value;
    else el.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

/** Shows a short message in the bottom-right corner. */
function toast(message, kind = "info", milliseconds = 5000) {
  const el = h("div", { class: `toast ${kind}` }, message);
  document.getElementById("toasts").append(el);
  setTimeout(() => el.remove(), milliseconds);
}

/* ====================================================================
   3. API HELPER
   ==================================================================== */

/** Error raised for any failed API call; `status` is the HTTP status code. */
class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

/**
 * Calls the JSON API. Always sends `X-Requested-With: portal`, the marker the
 * server requires on POST/PUT/DELETE as CSRF protection (see csrf_guard in
 * src/routes/mod.rs).
 */
async function api(method, url, body) {
  const options = { method, credentials: "same-origin", headers: { "X-Requested-With": "portal" } };
  if (body instanceof Blob) {
    // A file (logo / icon upload): sent as it is, not as JSON.
    options.headers["Content-Type"] = body.type || "application/octet-stream";
    options.body = body;
  } else if (body !== undefined) {
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(url, options);
  } catch {
    throw new ApiError("Cannot reach the portal. Check your network connection.", 0);
  }
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* the answer was not JSON */ }

  if (!response.ok) {
    // 401 while signed in = the session expired or the account was disabled.
    if (response.status === 401 && state.user && url !== "/api/login") sessionExpired();
    throw new ApiError((data && data.error) || text.slice(0, 200) || `Request failed (${response.status})`, response.status);
  }
  return data;
}

/** Shows an API error unless it was a 401 (the login screen already explains that). */
function toastError(error) {
  if (error && error.status === 401) return;
  toast(error.message || String(error), "error", 8000);
}

/** Back to the login form with an explanation. */
function sessionExpired() {
  clearThumbnails();
  state.user = null;
  state.notice = "Your session has ended. Please sign in again.";
  renderApp();
}

/* ====================================================================
   4. FORMATTING HELPERS
   ==================================================================== */

/** DICOM person name "DOE^JOHN^^^" -> "DOE, JOHN". */
function formatName(raw) {
  if (!raw) return "(no name)";
  const parts = raw.split("^").map((p) => p.trim()).filter(Boolean);
  return parts.length > 1 ? `${parts[0]}, ${parts.slice(1).join(" ")}` : parts[0] || raw;
}

/** DICOM date "20240517" -> "2024-05-17". Anything else is shown unchanged. */
function formatDicomDate(value) {
  return /^\d{8}$/.test(value || "") ? `${value.slice(0, 4)}-${value.slice(4, 6)}-${value.slice(6, 8)}` : value || "";
}

/** Unix seconds -> local date/time string. */
const formatTimestamp = (unixSeconds) => (unixSeconds ? new Date(unixSeconds * 1000).toLocaleString() : "never");

/** Coloured modality badge (colours defined in app.css through data-m). */
const chip = (modality) => h("span", { class: "chip", "data-m": modality, title: modalityName(modality) }, modality || "?");

/**
 * Full names of the DICOM modality codes (DICOM PS3.3, "Modality" attribute), used as
 * the tooltip of the modality tags in the Studies and Series lists.
 */
const MODALITY_NAMES = {
  AR: "Autorefraction", AU: "Audio", BDUS: "Bone densitometry (ultrasound)", BI: "Biomagnetic imaging",
  BMD: "Bone densitometry (X-ray)", CR: "Computed radiography", CT: "Computed tomography",
  DG: "Diaphanography", DOC: "Document", DX: "Digital radiography", ECG: "Electrocardiography",
  EPS: "Cardiac electrophysiology", ES: "Endoscopy", GM: "General microscopy", HD: "Hemodynamic waveform",
  IO: "Intra-oral radiography", IVOCT: "Intravascular optical coherence tomography", IVUS: "Intravascular ultrasound",
  KER: "Keratometry", KO: "Key object selection", LS: "Laser surface scan", MG: "Mammography",
  MR: "Magnetic resonance", NM: "Nuclear medicine", OAM: "Ophthalmic axial measurements",
  OCT: "Optical coherence tomography", OP: "Ophthalmic photography", OPT: "Ophthalmic tomography",
  OPV: "Ophthalmic visual field", OT: "Other", PLAN: "Plan", PR: "Presentation state",
  PT: "Positron emission tomography (PET)", PX: "Panoramic X-ray", REG: "Registration",
  RESP: "Respiratory waveform", RF: "Radio fluoroscopy", RG: "Radiographic imaging (conventional film)",
  RTDOSE: "Radiotherapy dose", RTIMAGE: "Radiotherapy image", RTPLAN: "Radiotherapy plan",
  RTRECORD: "Radiotherapy treatment record", RTSTRUCT: "Radiotherapy structure set", SEG: "Segmentation",
  SM: "Slide microscopy", SMR: "Stereometric relationship", SR: "Structured report",
  SRF: "Subjective refraction", STAIN: "Automated slide stainer", TG: "Thermography", US: "Ultrasound",
  VA: "Visual acuity", XA: "X-ray angiography", XC: "External-camera photography",
};

/**
 * Tooltip of a modality tag. The description an administrator gave to a modality button
 * for exactly this code wins (their wording, their language); otherwise the DICOM name.
 */
function modalityName(code) {
  if (!code) return "Modality not recorded";
  const button = modalityButtons().find((b) => b.codes.length === 1 && b.codes[0] === code && b.title);
  const name = button ? button.title : MODALITY_NAMES[code];
  return name ? `${code}: ${name}` : `${code}: modality code`;
}

/** Grey placeholder for empty values. */
const dash = () => h("span", { class: "muted" }, "-");

/* ====================================================================
   5. SHELL: LOGIN, NAVIGATION, SIGN OUT
   ==================================================================== */

/**
 * The tabs. `needs` is the permission required to see the tab. Everything for
 * administrators (users, DICOM nodes, audit log...) lives in Settings, which has
 * its own tabs (SETTINGS_TABS).
 */
const VIEWS = [
  // `wide`: table pages use the whole width of the window; the others (forms, long
  // explanations) keep a readable maximum width.
  { id: "studies", label: "Studies", needs: "read", wide: true, build: () => buildSearchView(STUDY_SEARCH) },
  { id: "series", label: "Series", needs: "read", wide: true, build: () => buildSearchView(SERIES_SEARCH) },
  { id: "upload", label: "Uploads", needs: "upload", build: buildUploadView },
  { id: "transfers", label: "Transfers", needs: "send", wide: true, build: buildTransfersView },
  { id: "shares", label: "Shares", needs: "share", wide: true, build: buildSharesView },
  { id: "settings", label: "Settings", needs: "admin", build: buildSettingsView },
  { id: "help", label: "Help", needs: null, build: buildHelpView },
];

/** Opens Settings on one of its tabs (e.g. "orthanc" from the Orthanc notice). */
function openSettings(tab) {
  state.view = "settings";
  state.simpleView = "settings";
  state.settingsTab = tab;
  renderApp();
}

/** Opens the list of studies, in whichever view the user has. */
function openStudies() {
  if (!can("read")) return;
  state.view = "studies";
  state.simpleView = "search";
  state.simpleSearch = "studies";
  renderApp();
}

/* ---- Normal or simplified view ----
   The server decides from the user's role (Settings > Appearance and Security & roles).
   Users with the switch_view permission may switch themselves; that choice is kept in this
   browser only (localStorage), per user name. */

const interfaceKey = () => `portal.interface.${state.user ? state.user.username : ""}`;

/**
 * The user's own choice of view, read from the browser's storage once and then kept in memory:
 * currentInterface() is asked by every study button while a table is drawn.
 */
const interfaceChoice = { key: null, value: "" };
function readInterfaceChoice() {
  const key = interfaceKey();
  if (interfaceChoice.key !== key) {
    let value = "";
    try { value = localStorage.getItem(key) || ""; } catch { /* storage blocked: no remembered choice */ }
    Object.assign(interfaceChoice, { key, value });
  }
  return interfaceChoice.value;
}

/** "normal" or "simplified": what this user sees now. */
function currentInterface() {
  const features = state.user && state.user.features;
  if (!features) return "normal";
  if (features.interface_switch) {
    const choice = readInterfaceChoice();
    if (choice === "normal" || choice === "simplified") return choice;
  }
  return features.interface === "simplified" ? "simplified" : "normal";
}

/** The "switch view" button (only for users with the switch_view permission). */
function switchInterface() {
  const next = currentInterface() === "simplified" ? "normal" : "simplified";
  Object.assign(interfaceChoice, { key: interfaceKey(), value: next });
  try { localStorage.setItem(interfaceKey(), next); } catch { /* this browser cannot remember it: still switch now */ }
  if (next === "simplified" && state.view === "settings") state.simpleView = "settings";
  if (next === "normal" && state.simpleView === "settings") state.view = "settings";
  renderApp();
}

/** Draws the whole page: login form when signed out, otherwise top bar + current tab. */
function renderApp() {
  root.replaceChildren();
  // Colours as saved (a preview in Settings > Appearance ends when the user leaves the page).
  applyTheme(state.brand.theme);
  if (!state.user) {
    root.append(buildLogin());
    return;
  }
  if (currentInterface() === "simplified") {
    renderSimplified();
    return;
  }

  // Only the tabs this user may use.
  const views = VIEWS.filter((v) => !v.needs || can(v.needs));
  if (!views.some((v) => v.id === state.view)) state.view = views.length ? views[0].id : null;
  const current = views.find((v) => v.id === state.view);

  const nav = h("nav", { class: "nav", "aria-label": "Main" },
    views.map((v) => h("button", {
      type: "button",
      "aria-current": v.id === state.view ? "page" : null,
      onclick: () => { state.view = v.id; renderApp(); },
    }, v.label, v.id === "upload" ? uploadBadge() : null)));

  const topbar = h("header", { class: "topbar" },
    h("div", { class: "brand brand-slot", "data-size": "24" }, ...brandContent(24, false)),
    nav,
    h("div", { class: "userbox" },
      h("span", null, state.user.username),
      state.user.features.interface_switch
        ? h("button", { class: "btn small", type: "button", onclick: switchInterface, title: "Switch to the simplified view (remembered in this browser)" }, "Simplified view")
        : null,
      h("button", { class: "btn small", type: "button", onclick: signOut }, "Sign out")));

  const page = h("main", { class: current && current.wide ? "page wide" : "page" },
    current ? current.build() : h("div", { class: "panel empty" }, "Your account has no permissions yet. Ask an administrator to grant you access."));
  // Red notice (administrators only) when Orthanc is unreachable or too old.
  const banner = h("div", { id: "orthanc-banner", class: "banner", role: "alert", hidden: true });
  root.append(topbar, banner, page);
  paintOrthancBanner();
  refreshOrthancStatus();
}

/**
 * What identifies the practice in the top bar and on the login page: the uploaded
 * logo (or the default mark) and, unless an administrator hid it, the portal name.
 * `asHeading` makes the name an <h1> (login page) instead of a <span> (top bar).
 */
function brandContent(size, asHeading) {
  const brand = state.brand;
  const mark = brand.logo
    ? h("img", { class: "brand-logo", src: brand.logo.url, alt: brand.show_name ? "" : brand.name })
    : h("img", { src: "/favicon.svg", alt: brand.show_name ? "" : brand.name, width: size, height: size });
  const name = brand.show_name ? h(asHeading ? "h1" : "span", { class: "brand-name" }, brand.name) : null;
  return [mark, name];
}

/* ---- Colours (Settings > Appearance) ----
   The stylesheet defines every colour as a CSS variable (app.css, sections 1 and 2). The colours
   an administrator picks are set on <html> through the DOM (allowed by the Content-Security-
   Policy, unlike inline style attributes), together with readable text colours computed here. */

/** Every variable applyTheme may set, so a colour put back to "default" is removed again. */
const THEME_VARS = ["--accent", "--accent-hover", "--on-accent", "--bar", "--bar-ink", "--bar-ink-strong", "--bar-line",
  "--bar-hover", "--header", "--header-ink", "--header-line", "--side", "--side-ink", "--side-ink-strong", "--side-hover",
  "--side-line", "--paper"];

const isColor = (value) => /^#[0-9a-f]{6}$/i.test(value || "");
const rgbOf = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));

/** Mixes two #rrggbb colours: `amount` 0 = `a`, 1 = `b`. */
function mixColors(a, b, amount) {
  const [x, y] = [rgbOf(a), rgbOf(b)];
  return "#" + x.map((v, i) => Math.round(v + (y[i] - v) * amount).toString(16).padStart(2, "0")).join("");
}

/** WCAG relative luminance (0 = black, 1 = white). */
function luminance(hex) {
  const [r, g, b] = rgbOf(hex).map((v) => { const c = v / 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** Dark or white text, whichever reads better on `background`. */
const inkOn = (background) => (luminance(background) > 0.179 ? "#15212c" : "#ffffff");

/** Applies the light/dark mode and the custom colours. `theme` = { mode, accent, header, sidebar, background }. */
function applyTheme(theme) {
  const html = document.documentElement;
  const t = theme || {};
  if (t.mode === "light" || t.mode === "dark") html.dataset.theme = t.mode; else delete html.dataset.theme;
  THEME_VARS.forEach((name) => html.style.removeProperty(name));
  const set = (name, value) => html.style.setProperty(name, value);

  html.classList.toggle("custom-accent", isColor(t.accent));
  if (isColor(t.accent)) {
    set("--accent", t.accent);
    // Hover: a little darker, or lighter when the colour is already very dark.
    set("--accent-hover", mixColors(t.accent, luminance(t.accent) < 0.04 ? "#ffffff" : "#000000", 0.18));
    set("--on-accent", inkOn(t.accent));
  }
  if (isColor(t.header)) {
    const ink = inkOn(t.header);
    set("--bar", t.header); set("--header", t.header);
    set("--bar-ink", mixColors(ink, t.header, 0.15)); set("--bar-ink-strong", ink); set("--header-ink", ink);
    set("--bar-line", mixColors(t.header, ink, 0.35)); set("--header-line", mixColors(t.header, ink, 0.35));
    set("--bar-hover", mixColors(t.header, ink, 0.12));
  }
  if (isColor(t.sidebar)) {
    const ink = inkOn(t.sidebar);
    set("--side", t.sidebar); set("--side-ink", mixColors(ink, t.sidebar, 0.25)); set("--side-ink-strong", ink);
    set("--side-hover", mixColors(t.sidebar, ink, 0.1)); set("--side-line", mixColors(t.sidebar, ink, 0.3));
  }
  if (isColor(t.background)) set("--paper", t.background);
}

/** Applies the identity everywhere it shows: browser tab title, website icon, logo and name. */
function applyBrand() {
  applyTheme(state.brand.theme);
  document.title = state.brand.title;
  let icon = document.querySelector('link[rel="icon"]');
  if (!icon) {
    icon = document.createElement("link");
    icon.rel = "icon";
    document.head.append(icon);
  }
  icon.setAttribute("href", state.brand.favicon.url);
  icon.setAttribute("type", state.brand.favicon.type);
  document.querySelectorAll(".brand-slot").forEach((slot) => {
    slot.replaceChildren(...brandContent(Number(slot.dataset.size), slot.dataset.heading === "1"));
  });
}

/** Reloads the identity from the server (after an administrator changes it). */
async function refreshBrand() {
  state.brand = await api("GET", "/api/public");
  applyBrand();
}

/** Login screen. */
function buildLogin() {
  const error = h("div", { class: "form-error", role: "alert", hidden: !state.notice }, state.notice);
  const username = h("input", { name: "username", autocomplete: "username", required: true, autofocus: true });
  const password = h("input", { name: "password", type: "password", autocomplete: "current-password", required: true });
  const submit = h("button", { class: "btn primary", type: "submit" }, "Sign in");

  const form = h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      submit.disabled = true;
      error.hidden = true;
      try {
        state.user = await api("POST", "/api/login", { username: username.value, password: password.value });
        state.notice = "";
        renderApp();
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
        password.value = "";
        password.focus();
      }
    },
  }, error, h("label", null, "Username", username), h("label", null, "Password", password), submit);

  return h("div", { class: "login-wrap" },
    h("section", { class: "panel login-card" },
      h("div", { class: "brand brand-slot", "data-size": "28", "data-heading": "1" }, ...brandContent(28, true)),
      h("p", { class: "muted" }, "Sign in to search, view and manage imaging studies."),
      state.brand.login_notice ? h("p", { class: "login-notice" }, state.brand.login_notice) : null,
      form));
}

async function signOut() {
  clearThumbnails();
  try { await api("POST", "/api/logout"); } catch { /* leaving anyway */ }
  state.user = null;
  state.notice = "";
  state.simpleView = "home";
  renderApp();
}

/* ====================================================================
   5b. SIMPLIFIED VIEW
   A reduced layout for the roles an administrator chooses (Settings > Appearance and
   Security & roles):

     +------------------------------------------------------+
     | logo + practice name                Total studies: N |
     +----+-------------------------------------------------+
     | ⌂  |  Home: recent studies (grid of cards)           |
     | ⌕  |                                                 |
     | <  |                                                 |
     | ☁  |  ---------------------------------------------- |
     |    |  Drop zone: drag files here to upload them      |
     | ⚙  |                                                 |
     | ⇥  |                                                 |
     +----+-------------------------------------------------+

   Every icon names itself when the mouse rests on it (or under it, if an administrator
   chose so). The pages behind the icons are the same builders as the normal view's tabs.
   On phones the icon column becomes a bar at the bottom of the screen (app.css).
   ==================================================================== */

const SVG_NS = "http://www.w3.org/2000/svg";

/** A gear outline (8 teeth) around (12, 12), computed once. */
const GEAR_PATH = (() => {
  const teeth = 8, outer = 10.4, inner = 7.9, base = 0.27, tip = 0.15;
  const point = (angle, r) => `${(12 + r * Math.cos(angle)).toFixed(2)} ${(12 + r * Math.sin(angle)).toFixed(2)}`;
  let d = "";
  for (let i = 0; i < teeth; i++) {
    const a = (i * 2 * Math.PI) / teeth;
    const next = ((i + 1) * 2 * Math.PI) / teeth;
    d += `${i === 0 ? "M" : "L"}${point(a - base, inner)} L${point(a - tip, outer)} L${point(a + tip, outer)} L${point(a + base, inner)} `;
    d += `A${inner} ${inner} 0 0 1 ${point(next - base, inner)} `;
  }
  return d + "Z";
})();

/** Line icons on a 24 x 24 grid: path data, or `{ c: [cx, cy, r] }` for a circle. */
const ICONS = {
  home: ["M3 11.5 12 4l9 7.5", "M5.5 9.8V20h13V9.8", "M10 20v-5.5h4V20"],
  search: [{ c: [10.5, 10.5, 6.5] }, "M15.4 15.4 21 21"],
  share: [{ c: [18, 5.5, 2.5] }, { c: [6, 12, 2.5] }, { c: [18, 18.5, 2.5] }, "M8.2 10.8l7.6-4.1", "M8.2 13.2l7.6 4.1"],
  upload: ["M7 18.5H6.5a4.5 4.5 0 0 1-.4-8.98 6 6 0 0 1 11.6-1.2A4.5 4.5 0 0 1 17.5 18.5H17", "M12 21v-8.5", "M8.8 15.5 12 12.3l3.2 3.2"],
  settings: [GEAR_PATH, { c: [12, 12, 3.2] }],
  logout: ["M13 3.5h6v17h-6", "M3.5 12h11", "M11 8.5l3.5 3.5-3.5 3.5"],
  swap: ["M4 8h14", "M15 5l3 3-3 3", "M20 16H6", "M9 13l-3 3 3 3"],
  transfers: ["M3 12h10.5", "M10 8.5l3.5 3.5-3.5 3.5", "M16 4.5h5v15h-5z", "M18.5 8h.01", "M18.5 11h.01"], // an arrow into another system
  help: [{ c: [12, 12, 9] }, "M9.6 9.4a2.5 2.5 0 1 1 3.4 2.4c-.6.3-1 .8-1 1.5v.6", { c: [12, 16.9, 0.35] }],
  // Buttons of a study (simplified view)
  weasis: ["M3 4.5h18v11.5H3z", "M8.5 20.5h7", "M12 16v4.5"],                       // a desktop screen
  ohif: ["M3 5h18v14H3z", "M3 9h18", { c: [5.5, 7, 0.4] }, { c: [7.5, 7, 0.4] }, "M8 14.5s1.6-2.5 4-2.5 4 2.5 4 2.5-1.6 2.5-4 2.5-4-2.5-4-2.5z"], // a browser window with an eye
  send: ["M21 3 10.5 13.5", "M21 3l-6.5 18-4-7.5L3 9.5z"],                          // a paper plane
  download: ["M12 3.5v11.5", "M7.5 10.5 12 15l4.5-4.5", "M4 17v3.5h16V17"],
  refresh: ["M19.5 12a7.5 7.5 0 1 1-2.2-5.3", "M19.5 4v4.5H15"],
  link: ["M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1", "M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"],
  revoke: [{ c: [12, 12, 8.5] }, "M6 6l12 12"],
  key: [{ c: [7.5, 15.5, 4] }, "M10.5 12.5 20 3", "M16.5 6.5l2.5 2.5", "M14 9l2 2"],
  edit: ["M4 20h4L19 9l-4-4L4 16z", "M13.5 6.5l4 4"],
};

/**
 * Should the buttons of a study be icons? Yes in the simplified view (each one names itself in a
 * tooltip), text in the normal view.
 */
const iconButtons = () => !!state.user && currentInterface() === "simplified";

/**
 * Turns the properties of a text button into those of an icon button: the label becomes the
 * tooltip (see the tooltip code below) and the accessible name.
 */
const asIcon = (props, label) => ({ ...props, class: `${props.class || ""} icon-btn`.trim(), "data-tip": label, "aria-label": label, title: null });

/**
 * A study button: text in the normal view, an icon with a tooltip in the simplified view.
 * `tag` is "button" or "a"; `tip` is the (longer) description shown on hover.
 */
function actionButton(tag, iconName, text, tip, props) {
  return iconButtons()
    ? h(tag, asIcon(props, tip), icon(iconName))
    : h(tag, { ...props, title: props.title || null }, text);
}

/* ---- Tooltips of the icon buttons ----
   One floating box, placed above the hovered or focused button (below it near the top of the
   window) and kept inside the window. It lives outside the tables, so their scroll boxes never
   clip it; inside a dialog it moves into the dialog, which is drawn above everything else. */
const tipBox = h("div", { class: "tooltip", role: "tooltip", hidden: true });

let tipFor = null; // the button the tooltip is showing for
function showTip(target) {
  // The mouse crossing the icon's lines fires mouseover again: nothing to redo then.
  if (tipFor === target && !tipBox.hidden) return;
  tipFor = target;
  const host = target.closest("dialog") || document.body;
  if (tipBox.parentNode !== host) host.append(tipBox);
  tipBox.textContent = target.dataset.tip;
  tipBox.hidden = false;
  const r = target.getBoundingClientRect();
  const t = tipBox.getBoundingClientRect();
  let top = r.top - t.height - 6;
  if (top < 4) top = r.bottom + 6;
  const left = Math.min(Math.max(4, r.left + r.width / 2 - t.width / 2), window.innerWidth - t.width - 4);
  tipBox.style.top = `${Math.round(top)}px`;
  tipBox.style.left = `${Math.round(left)}px`;
}
const hideTip = () => { tipBox.hidden = true; tipFor = null; };
const tipTarget = (event) => (event.target instanceof Element ? event.target.closest(".icon-btn[data-tip], .has-tip[data-tip]") : null);
document.addEventListener("mouseover", (event) => { const t = tipTarget(event); if (t) showTip(t); else hideTip(); });
document.addEventListener("focusin", (event) => { const t = tipTarget(event); if (t && t.matches(":focus-visible")) showTip(t); else hideTip(); });
document.addEventListener("focusout", hideTip);
document.addEventListener("click", hideTip);
document.addEventListener("scroll", hideTip, true);

/** An <svg> icon from ICONS (decorative: the button around it carries the name). */
function icon(name) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const part of ICONS[name]) {
    const el = document.createElementNS(SVG_NS, typeof part === "string" ? "path" : "circle");
    if (typeof part === "string") el.setAttribute("d", part);
    else ["cx", "cy", "r"].forEach((attr, i) => el.setAttribute(attr, part.c[i]));
    svg.append(el);
  }
  return svg;
}

/**
 * The pages of the simplified view, top to bottom. `needs` = permission required to see the
 * icon (null = everyone). `bottom` pages sit at the foot of the column, above "Log out".
 */
const SIMPLE_PAGES = [
  { id: "home", label: "Home", icon: "home", needs: null, build: buildHomeView },
  { id: "search", label: "Search", icon: "search", needs: "read", build: buildSimpleSearch },
  { id: "shares", label: "Shares", icon: "share", needs: "share", build: buildSharesView },
  { id: "upload", label: "Uploads", icon: "upload", needs: "upload", build: buildUploadView },
  { id: "transfers", label: "Transfers", icon: "transfers", needs: "send", build: buildTransfersView },
  { id: "help", label: "Help", icon: "help", needs: null, bottom: true, build: buildHelpView },
  { id: "settings", label: "Configuration", icon: "settings", needs: "admin", bottom: true, build: buildSettingsView },
];

/**
 * Search page of the simplified view: the Studies and the Series searches of the normal view
 * (its two tabs), behind two small tabs of their own.
 */
function buildSimpleSearch() {
  const kinds = [["studies", "Studies", STUDY_SEARCH], ["series", "Series", SERIES_SEARCH]];
  if (!kinds.some(([id]) => id === state.simpleSearch)) state.simpleSearch = "studies";
  const tabs = h("div", { class: "subtabs", role: "tablist", "aria-label": "Search" },
    kinds.map(([id, label]) => h("button", {
      type: "button", role: "tab", "aria-selected": String(id === state.simpleSearch),
      onclick: () => { state.simpleSearch = id; renderApp(); },
    }, label)));
  const view = buildSearchView(kinds.find(([id]) => id === state.simpleSearch)[2]);
  view.insertBefore(tabs, view.children[1]); // under the page title, above the filters
  return view;
}

/** Options of the simplified view sent by the server (Settings > Appearance). */
const simpleOptions = () => (state.user && state.user.features && state.user.features.simplified) || {};

/** Draws the simplified view: header, icon column and the current page. */
function renderSimplified() {
  const options = simpleOptions();
  const pages = SIMPLE_PAGES.filter((p) => !p.needs || can(p.needs));
  if (!pages.some((p) => p.id === state.simpleView)) state.simpleView = "home";
  const current = pages.find((p) => p.id === state.simpleView);

  /** One icon of the column. `data-tip` is the description that pops up on hover (app.css). */
  const sideButton = (label, iconName, onclick, isCurrent = false, extra = null) => h("button", {
    type: "button", class: "side-btn", "data-tip": label, "aria-label": label,
    "aria-current": isCurrent ? "page" : null, onclick,
  }, icon(iconName), h("span", { class: "side-label", "aria-hidden": "true" }, label), extra);
  const pageButton = (p) => sideButton(p.label, p.icon, () => { state.simpleView = p.id; renderApp(); }, p.id === current.id,
    p.id === "upload" ? uploadBadge() : null);

  const side = h("nav", { class: "simple-side", "aria-label": "Main" },
    h("div", { class: "side-group" }, pages.filter((p) => !p.bottom).map(pageButton)),
    h("div", { class: "side-group" },
      state.user.features.interface_switch ? sideButton("Normal view", "swap", switchInterface) : null,
      pages.filter((p) => p.bottom).map(pageButton),
      sideButton(`Log out (${state.user.username})`, "logout", signOut)));

  const showTotal = options.show_total !== false && can("read");
  const head = h("header", { class: "simple-head" },
    h("div", { class: "brand brand-slot", "data-size": "40" }, ...brandContent(40, false)),
    showTotal ? h("div", { class: "simple-total" }, "Total studies: ", h("b", { id: "total-studies" }, totalStudies.text)) : null);

  // Red notice (administrators only) when Orthanc is unreachable or too old.
  const banner = h("div", { id: "orthanc-banner", class: "banner", role: "alert", hidden: true });
  const main = h("main", { class: "simple-main" }, banner, current.build());

  root.append(h("div", { class: options.show_labels ? "simple labels" : "simple" }, head, side, main));
  paintOrthancBanner();
  refreshOrthancStatus();
  if (showTotal) refreshTotalStudies();
}

/**
 * Fills in "Total studies: N" in the header of the simplified view. The header is redrawn at every
 * click on an icon, so the number is asked from Orthanc at most once a minute, unless `force`
 * (after an upload).
 */
const totalStudies = { text: "...", at: 0 };
async function refreshTotalStudies(force = false) {
  const show = () => { const slot = document.getElementById("total-studies"); if (slot) slot.textContent = totalStudies.text; };
  show();
  if (!document.getElementById("total-studies") || (!force && Date.now() - totalStudies.at < 60000)) return;
  totalStudies.at = Date.now();
  try {
    const stats = await api("GET", "/api/stats");
    totalStudies.text = typeof stats.studies === "number" ? stats.studies.toLocaleString() : "?";
  } catch {
    totalStudies.text = "?"; // Orthanc unreachable: the administrators' red notice explains it
    totalStudies.at = 0;     // try again at the next page
  }
  show();
}

/** Reloads the recent studies of the home page, if it is on screen (set by buildHomeView). */
let reloadRecentStudies = null;

/** Home page of the simplified view: the most recent studies, and a drop zone to upload. */
function buildHomeView() {
  const options = simpleOptions();
  const parts = [];

  if (can("read")) {
    const body = h("div", { class: "recent-body" }, h("div", { class: "loading" }, "Loading..."));
    let latest = 0; // answers can arrive out of order: only the newest request may draw

    async function load() {
      const thisLoad = ++latest;
      const params = new URLSearchParams({ limit: options.recent_count || 12, offset: 0 });
      if (options.recent_order === "received") params.set("order", "received");
      try {
        const page = await api("GET", `/api/studies?${params}`);
        if (thisLoad !== latest) return;
        if (!page.items.length) body.replaceChildren(h("div", { class: "empty" }, "No study in the PACS yet."));
        // The same table as the Studies page (same columns, same buttons), or cards (Settings > Appearance).
        else if (options.recent_layout === "cards") body.replaceChildren(h("div", { class: "study-grid", "data-size": options.card_size || "medium" }, page.items.map(studyCard)));
        else body.replaceChildren(renderStudiesTable(page.items));
      } catch (e) {
        if (thisLoad !== latest) return;
        body.replaceChildren(h("div", { class: "empty" }, e.message));
        toastError(e);
      }
    }

    const section = h("section", { class: "recent", "aria-labelledby": "recent-title" },
      h("div", { class: "recent-head" },
        h("h2", { id: "recent-title" }, options.recent_title || "Recent studies"),
        actionButton("button", "refresh", "Refresh", "Refresh the recent studies", { class: "btn small", type: "button", onclick: load })),
      body);
    reloadRecentStudies = () => { if (section.isConnected) load(); };
    load();
    parts.push(section);
  }

  if (can("upload")) {
    const dropzone = buildDropzone({ title: options.upload_text || "Drag a file to upload it", home: true });
    parts.push(dropzone, buildUploadProgress());
  }

  if (!parts.length) parts.push(h("div", { class: "panel empty" }, "Your account has no permissions yet. Ask an administrator to grant you access."));
  return h("div", { class: "home" }, parts);
}

/** One study of the home page: a card that opens the study's details. */
function studyCard(study) {
  const name = formatName(study.patient_name);
  const date = formatDicomDate(study.study_date);
  const open = () => openStudyDialog(study);
  return h("div", {
    class: "study-card", role: "button", tabindex: "0",
    "aria-label": [name, date, study.study_description].filter(Boolean).join(", "),
    onclick: open,
    onkeydown: (event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); } },
  },
    showThumbs() ? thumb("studies", study.id, study.modalities[0], false, 256) : null,
    h("div", { class: "card-title", title: name }, name),
    h("div", { class: "card-sub" }, [date, study.patient_id].filter(Boolean).join("  ·  ") || "-"),
    study.study_description ? h("div", { class: "card-sub", title: study.study_description }, study.study_description) : null,
    study.modalities.length ? h("div", { class: "card-chips" }, study.modalities.map(chip)) : null);
}

/** Details of one study (opened from a card): who, when, what, the actions and the series. */
function openStudyDialog(study) {
  const seriesSlot = h("div", null, h("div", { class: "loading" }, "Loading series..."));
  const demographics = [study.patient_sex, formatDicomDate(study.patient_birth_date)].filter(Boolean).join(", ");
  const detail = (label, value) => (value ? [h("dt", null, label), h("dd", null, value)] : null);
  const dialog = h("dialog", { class: "study-dialog" }, h("div", { class: "dialog-form" },
    h("h2", null, formatName(study.patient_name)),
    h("dl", { class: "share-details" },
      detail("Patient ID", study.patient_id),
      detail("Sex, birth date", demographics),
      detail("Study date", formatDicomDate(study.study_date)),
      detail("Description", study.study_description),
      detail("Modalities", study.modalities.length ? h("span", null, study.modalities.map(chip)) : null),
      detail("Content", `${study.series_count ?? "?"} series${study.instances_count != null ? `, ${study.instances_count} images` : ""}`)),
    h("div", { class: "actions" }, studyActions(study)),
    seriesSlot,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();

  api("GET", `/api/studies/${study.id}/series`)
    .then((list) => seriesSlot.replaceChildren(renderSeriesBox(list)))
    .catch((e) => seriesSlot.replaceChildren(h("div", { class: "series-empty muted" }, e.message)));
}

/* ====================================================================
   6b. PREVIEW THUMBNAILS
   Each thumbnail is a small box that loads its picture only when it scrolls
   into view, a few at a time. Pictures are kept as in-memory object URLs for
   this page session only (never in the browser's disk cache: the server sends
   them with Cache-Control: no-store) and are discarded when the user signs out.
   ==================================================================== */

/** Are thumbnails shown for this user? They need `visualize` and the server setting. */
const showThumbs = () => can("visualize") && !!(state.user && state.user.features && state.user.features.thumbnails);

const THUMB_PARALLEL = 4;            // pictures requested at the same time
const THUMB_CACHE_MAX = 400;         // pictures remembered for this page session (oldest are dropped)
const thumbImages = new Map();       // thumbnail URL -> object URL ("" = this item has no preview)
const thumbQueue = [];               // boxes waiting for their picture
let thumbRunning = 0;

/** Watches the boxes and queues each one when it is about to become visible. */
const thumbObserver = "IntersectionObserver" in window
  ? new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) { thumbObserver.unobserve(entry.target); queueThumb(entry.target); }
      }
    }, { rootMargin: "200px" })
  : null;

/** Puts the picture in the box, or marks it as "no preview" (the modality label stays). */
function paintThumb(box, objectUrl) {
  box.classList.remove("loading");
  if (objectUrl) box.replaceChildren(h("img", { src: objectUrl, alt: "" }));
  else box.classList.add("empty");
}

/** Remembers a picture, forgetting the oldest ones beyond THUMB_CACHE_MAX (and freeing their memory). */
function rememberThumb(url, objectUrl) {
  thumbImages.set(url, objectUrl);
  while (thumbImages.size > THUMB_CACHE_MAX) {
    const [oldUrl, oldObject] = thumbImages.entries().next().value;
    thumbImages.delete(oldUrl);
    if (oldObject) URL.revokeObjectURL(oldObject);
  }
}

/**
 * A remembered picture, moved to the "most recently used" end of the list, so the ones
 * dropped beyond THUMB_CACHE_MAX are those nobody looked at for the longest time.
 */
function cachedThumb(url) {
  const objectUrl = thumbImages.get(url);
  thumbImages.delete(url);
  thumbImages.set(url, objectUrl);
  return objectUrl;
}

/** Downloads one thumbnail. Resolves to an object URL, or "" when there is no preview. */
async function loadThumb(url) {
  if (thumbImages.has(url)) return cachedThumb(url);
  try {
    const response = await fetch(url, { credentials: "same-origin" });
    if (response.ok) {
      const objectUrl = URL.createObjectURL(await response.blob());
      rememberThumb(url, objectUrl);
      return objectUrl;
    }
    // 404 = the server says there is nothing to show: remember it. Other errors
    // (session ended, server busy) are not remembered, so a later render retries.
    if (response.status === 404) rememberThumb(url, "");
  } catch { /* network problem: leave the placeholder */ }
  return "";
}

function queueThumb(box) {
  if (thumbImages.has(box.thumbUrl)) { paintThumb(box, cachedThumb(box.thumbUrl)); return; }
  thumbQueue.push(box);
  setTimeout(pumpThumbs, 0); // after the table has been attached to the page
}

/** Keeps up to THUMB_PARALLEL requests running; skips boxes that left the page. */
function pumpThumbs() {
  while (thumbRunning < THUMB_PARALLEL && thumbQueue.length) {
    const box = thumbQueue.shift();
    if (!box.isConnected) continue; // the results were replaced meanwhile
    thumbRunning++;
    loadThumb(box.thumbUrl)
      .then((objectUrl) => paintThumb(box, objectUrl))
      .finally(() => { thumbRunning--; pumpThumbs(); });
  }
}

/** Frees the pictures (sign-out, expired session). */
function clearThumbnails() {
  for (const objectUrl of thumbImages.values()) if (objectUrl) URL.revokeObjectURL(objectUrl);
  thumbImages.clear();
  thumbQueue.length = 0;
}

/**
 * A thumbnail box. `kind` is "studies" or "series" and `id` the Orthanc id.
 * Until the picture arrives (or if there is none) the box shows the modality code.
 * `size` asks the server for a bigger picture (the cards of the simplified view).
 */
function thumb(kind, id, modality, small = false, size = 0) {
  const box = h("div", { class: small ? "thumb small loading" : "thumb loading", "aria-hidden": "true" },
    h("span", { class: "thumb-label" }, modality || ""));
  box.thumbUrl = `/api/${kind}/${id}/thumbnail${size ? `?size=${size}` : ""}`;
  if (thumbObserver) thumbObserver.observe(box); else queueThumb(box);
  return box;
}

/* ====================================================================
   6. STUDIES AND SERIES SEARCH
   ==================================================================== */

/** Remembers what the user typed in each search form while they browse other tabs. */
const searchMemory = { studies: {}, series: {} };

/** Configuration of the "Studies" tab. `fields` become the filter form. */
const STUDY_SEARCH = {
  id: "studies",
  title: "Studies",
  endpoint: "/api/studies",
  fields: [
    { name: "patient_name", label: "Patient name", placeholder: "DOE or DOE*" },
    { name: "patient_id", label: "Patient ID" },
    { name: "description", label: "Study description" },
    { name: "modality", label: "Modality", type: "modalities" },
    { name: "date_from", label: "Study date from", type: "date" },
    { name: "date_to", label: "Study date to", type: "date" },
  ],
  renderTable: renderStudiesTable,
};

/** Configuration of the "Series" tab. */
const SERIES_SEARCH = {
  id: "series",
  title: "Series",
  endpoint: "/api/series",
  fields: [
    { name: "patient_name", label: "Patient name", placeholder: "DOE or DOE*" },
    { name: "patient_id", label: "Patient ID" },
    { name: "description", label: "Series description" },
    { name: "modality", label: "Modality", type: "modalities" },
    { name: "body_part", label: "Body part", placeholder: "CHEST" },
    { name: "date_from", label: "Study date from", type: "date" },
    { name: "date_to", label: "Study date to", type: "date" },
  ],
  renderTable: renderSeriesTable,
};

/**
 * Modality filter: a row of toggle buttons (CT, MR, US, RX... as set up by an
 * administrator) plus a box for any other DICOM code, which is ALWAYS there (even when
 * every button is hidden). Several can be active at once ("CT or US"). It exposes
 * `.value` (comma-separated codes) like an <input>, so the generic search code can
 * read, store and clear it. `onChange` is called when a button is toggled.
 * `chips` defaults to the buttons of this installation (the Settings preview passes its own).
 */
function buildModalityPicker(initial, onChange, chips = modalityButtons()) {
  const known = new Set(chips.flatMap((chip) => chip.codes));
  const selected = new Set();          // active codes that have a button
  const other = h("input", { class: "mod-other", placeholder: "Other codes: SR, ES...", "aria-label": "Other modality codes" });
  const buttons = chips.map((chip) => ({
    chip,
    button: h("button", {
      type: "button", class: "chip-toggle", title: chip.title, "aria-pressed": "false",
      onclick: () => {
        const active = chip.codes.every((c) => selected.has(c));
        chip.codes.forEach((c) => (active ? selected.delete(c) : selected.add(c)));
        refresh();
        onChange();
      },
    }, chip.label),
  }));

  /** Lights up the buttons whose codes are all selected. */
  function refresh() {
    for (const { chip, button } of buttons) {
      button.setAttribute("aria-pressed", String(chip.codes.every((c) => selected.has(c))));
    }
  }
  /** Loads a comma-separated list: known codes light buttons, the rest go to the box. */
  function load(list) {
    const codes = list.split(",").map((c) => c.trim().toUpperCase()).filter(Boolean);
    selected.clear();
    codes.filter((c) => known.has(c)).forEach((c) => selected.add(c));
    other.value = codes.filter((c) => !known.has(c)).join(", ");
    refresh();
  }

  load(initial);
  return {
    node: h("div", { class: "mod-picker" }, buttons.map((b) => b.button), other),
    get value() {
      const extra = other.value.split(/[\s,;]+/).map((c) => c.toUpperCase()).filter(Boolean);
      return [...new Set([...selected, ...extra])].join(",");
    },
    set value(list) { load(list); },
  };
}

/**
 * Generic search page: filter form + results panel + pagination.
 * `cfg` (STUDY_SEARCH / SERIES_SEARCH) says which fields to show, which API
 * endpoint to call and how to draw the rows.
 */
function buildSearchView(cfg) {
  const memory = searchMemory[cfg.id];
  const inputs = {};
  let offset = 0;
  let latestSearch = 0; // number of the newest search; older answers are ignored (see run())

  // One labelled control per field, pre-filled from memory. The modality field is
  // a group of toggle buttons that searches again as soon as one is toggled.
  const fieldNodes = cfg.fields.map((field) => {
    if (field.type === "modalities") {
      const picker = buildModalityPicker(memory[field.name] || "", () => run(0));
      inputs[field.name] = picker; // exposes .value like a normal input
      return h("div", { class: "field wide" }, h("span", { class: "field-label" }, field.label), picker.node);
    }
    const input = h("input", { name: field.name, type: field.type || "text", value: memory[field.name] || "", placeholder: field.placeholder || "" });
    inputs[field.name] = input;
    return h("label", null, field.label, input);
  });

  const results = h("section", { class: "panel" });

  /** Calls the API with the current filters and draws one page of results. */
  async function run(newOffset) {
    offset = newOffset;
    const thisSearch = ++latestSearch;
    const params = new URLSearchParams();
    for (const [name, input] of Object.entries(inputs)) {
      memory[name] = input.value;
      if (input.value.trim()) params.set(name, input.value.trim());
    }
    params.set("limit", PAGE_SIZE);
    params.set("offset", offset);

    results.replaceChildren(h("div", { class: "loading" }, "Searching..."));
    try {
      const page = await api("GET", `${cfg.endpoint}?${params}`);
      // The user may have toggled another filter while this one was still loading:
      // answers can arrive out of order, and an old one must never replace a newer one.
      if (thisSearch !== latestSearch) return;
      if (!page.items.length) {
        results.replaceChildren(h("div", { class: "empty" }, offset > 0 ? "No more results." : "No results. Try fewer filters, or a wildcard such as DOE*."));
        return;
      }
      results.replaceChildren(cfg.renderTable(page.items), buildPager(page));
    } catch (e) {
      if (thisSearch !== latestSearch) return;
      results.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  /** "Showing 1-25   [Previous] [Next]". */
  function buildPager(page) {
    return h("div", { class: "pager" },
      h("span", { class: "muted" }, `Showing ${offset + 1}-${offset + page.items.length}`),
      h("div", { class: "actions" },
        h("button", { class: "btn small", type: "button", disabled: offset === 0, onclick: () => run(Math.max(0, offset - PAGE_SIZE)) }, "Previous"),
        h("button", { class: "btn small", type: "button", disabled: !page.has_more, onclick: () => run(offset + PAGE_SIZE) }, "Next")));
  }

  const form = h("form", {
    class: "panel filters",
    onsubmit: (event) => { event.preventDefault(); run(0); },
  },
    h("div", { class: "filter-grid" }, fieldNodes),
    h("div", { class: "filter-actions" },
      h("button", { class: "btn primary", type: "submit" }, `Search ${cfg.title.toLowerCase()}`),
      h("button", {
        class: "btn", type: "button",
        onclick: () => { for (const input of Object.values(inputs)) input.value = ""; run(0); },
      }, "Clear filters")));

  run(0); // show the most recent studies right away
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, cfg.title)), form, results);
}

/** Results table of the "Studies" tab. Each study row can expand to show its series. */
const studyColumns = () => ["", ...(showThumbs() ? ["Preview"] : []), "Patient", "Patient ID", "Study date", "Description", "Modality", "Series / images", "Actions"];

function renderStudiesTable(studies) {
  const headers = studyColumns();
  return h("div", { class: "table-wrap" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null, headers.map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
      h("tbody", null, studies.map(studyRows))));
}

/** Two table rows per study: the study itself and a hidden row that will hold its series. */
function studyRows(study) {
  const seriesCell = h("td", { colspan: studyColumns().length }); // the whole width of the table
  const seriesRow = h("tr", { class: "series-row", hidden: true }, seriesCell);
  let loaded = false;

  // Expand / collapse arrow. The series are fetched the first time it opens.
  const twisty = h("button", {
    class: "twisty", type: "button", "aria-expanded": "false", "aria-label": "Show series",
    onclick: async () => {
      const open = twisty.getAttribute("aria-expanded") !== "true";
      twisty.setAttribute("aria-expanded", String(open));
      seriesRow.hidden = !open;
      if (!open || loaded) return;
      loaded = true;
      seriesCell.replaceChildren(h("div", { class: "loading" }, "Loading series..."));
      try {
        seriesCell.replaceChildren(renderSeriesBox(await api("GET", `/api/studies/${study.id}/series`)));
      } catch (e) {
        loaded = false; // allow a retry
        seriesCell.replaceChildren(h("div", { class: "series-empty muted" }, e.message));
      }
    },
  }, "\u25B8");

  const demographics = [study.patient_sex, formatDicomDate(study.patient_birth_date)].filter(Boolean).join(", ");
  const imageInfo = study.instances_count != null ? `${study.instances_count} images` : "";

  const mainRow = h("tr", { class: "study" },
    h("td", null, twisty),
    showThumbs() ? h("td", { class: "thumb-cell" }, thumb("studies", study.id, study.modalities[0])) : null,
    h("td", null, h("div", { class: "patient" }, formatName(study.patient_name)), demographics ? h("div", { class: "sub" }, demographics) : null),
    h("td", { class: "mono" }, study.patient_id || dash()),
    h("td", { class: "nowrap" }, formatDicomDate(study.study_date) || dash()),
    h("td", null, study.study_description || dash()),
    h("td", null, study.modalities.length ? study.modalities.map(chip) : dash()),
    h("td", null, `${study.series_count ?? "?"} series`, imageInfo ? h("div", { class: "sub" }, imageInfo) : null),
    h("td", null, h("div", { class: "actions" }, studyActions(study))));

  return [mainRow, seriesRow];
}

/** Buttons shown for a study, according to the user's permissions. */
function studyActions(study) {
  return [
    can("visualize") ? viewButtons(study) : null,
    can("send") ? actionButton("button", "send", "Send to...", "Send to a DICOM node", { class: "btn small", type: "button", onclick: () => openSendDialog(study) }) : null,
    can("share") ? actionButton("button", "share", "Share...", "Share with somebody outside (link + password)", { class: "btn small", type: "button", onclick: () => openShareDialog(study) }) : null,
    can("download") ? downloadMenu("studies", study.id) : null,
  ];
}

/** Small table with the series of one study (shown under the study row). */
function renderSeriesBox(seriesList) {
  if (!seriesList.length) return h("div", { class: "series-empty muted" }, "This study contains no series.");
  return h("div", { class: "series-box" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null, [...(showThumbs() ? ["Preview"] : []), "#", "Modality", "Description", "Body part", "Images", ""].map((t) => h("th", { class: t === "Images" ? "num" : null }, t)))),
      h("tbody", null, seriesList.map((s) => h("tr", null,
        showThumbs() ? h("td", { class: "thumb-cell" }, thumb("series", s.id, s.modality, true)) : null,
        h("td", null, s.series_number || dash()),
        h("td", null, chip(s.modality)),
        h("td", null, s.description || dash()),
        h("td", null, s.body_part || dash()),
        h("td", { class: "num" }, s.instances_count),
        h("td", null, h("div", { class: "actions" }, can("download") ? downloadMenu("series", s.id) : null)))))));
}

/** Results table of the "Series" tab. Each series shows the patient/study it belongs to. */
function renderSeriesTable(seriesList) {
  const headers = [...(showThumbs() ? ["Preview"] : []), "Patient", "Study date", "Study", "#", "Modality", "Series description", "Body part", "Images", "Actions"];
  return h("div", { class: "table-wrap" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null, headers.map((t) => h("th", { class: t === "Actions" || t === "Images" ? "num" : null }, t)))),
      h("tbody", null, seriesList.map((s) => {
        const study = s.study; // may be missing if Orthanc could not return the parent study
        return h("tr", { class: "study" },
          showThumbs() ? h("td", { class: "thumb-cell" }, thumb("series", s.id, s.modality)) : null,
          h("td", null, h("div", { class: "patient" }, study ? formatName(study.patient_name) : dash()), study ? h("div", { class: "sub mono" }, study.patient_id) : null),
          h("td", { class: "nowrap" }, study ? formatDicomDate(study.study_date) : dash()),
          h("td", null, (study && study.study_description) || dash()),
          h("td", null, s.series_number || dash()),
          h("td", null, chip(s.modality)),
          h("td", null, s.description || dash()),
          h("td", null, s.body_part || dash()),
          h("td", { class: "num" }, s.instances_count),
          h("td", null, h("div", { class: "actions" }, [
            can("visualize") && study ? viewButtons(study) : null,
            can("download") ? downloadMenu("series", s.id) : null,
          ])));
      }))));
}

/* ====================================================================
   7. WEASIS AND DOWNLOADS
   ==================================================================== */

/** Closes the drop-down menu (details element) that contains the clicked item. */
function closeMenu(event) {
  const menu = event.currentTarget.closest("details");
  if (menu) menu.removeAttribute("open");
}

/**
 * Asks the server for a `weasis://...` link containing a short-lived access
 * token and hands it to the browser, which starts Weasis on this computer (the
 * browser may ask for confirmation the first time).
 */
async function openInWeasis(studyUid, button) {
  if (!studyUid) { toast("This study has no StudyInstanceUID.", "error"); return; }
  button.disabled = true;
  try {
    const link = await api("GET", `/api/weasis/link?study_uid=${encodeURIComponent(studyUid)}`);
    toast("Opening Weasis. If nothing happens, check that Weasis is installed on this computer.", "info", 8000);
    window.location.href = link.uri;
  } catch (e) {
    toastError(e);
  } finally {
    button.disabled = false;
  }
}

/** Address of the OHIF web viewer for a study (served by Orthanc's OHIF plugin through the portal). */
const ohifUrl = (studyUid) => `/ohif/viewer?StudyInstanceUIDs=${encodeURIComponent(studyUid)}`;

/**
 * The two viewer buttons of a study, each shown only when it is switched on in
 * Settings > General > Viewers and uploads (OHIF also needs Orthanc's plugin when
 * the setting is "automatic").
 */
function viewButtons(study) {
  const buttons = [];
  if (hasWeasis()) {
    const weasis = actionButton("button", "weasis", "Weasis", "Open in Weasis (desktop viewer, must be installed on this computer)",
      { class: "btn small", type: "button", title: "Open in Weasis (desktop viewer, must be installed on this computer)" });
    weasis.addEventListener("click", () => openInWeasis(study.study_instance_uid, weasis));
    buttons.push(weasis);
  }
  if (hasOhif()) {
    buttons.push(actionButton("a", "ohif", "OHIF", "Open in the browser viewer (OHIF): new tab, nothing to install", {
      class: "btn small", href: ohifUrl(study.study_instance_uid), target: "_blank", rel: "noopener",
      title: "Open in the browser viewer (OHIF): new tab, nothing to install",
    }));
  }
  return buttons;
}

/**
 * "Download" drop-down for a study or series (`kind` = "studies" | "series").
 * Plain links: the browser itself downloads the ZIP, streamed from the server,
 * so even very large studies work and the browser shows the progress.
 */
function downloadMenu(kind, id) {
  return h("details", { class: "menu" },
    actionButton("summary", "download", "Download", kind === "series" ? "Download this series (ZIP)" : "Download the study (ZIP)", { class: "btn small" }),
    h("div", { class: "menu-list" },
      h("a", { class: "menu-item", href: `/api/${kind}/${id}/download`, onclick: closeMenu }, "ZIP", h("small", null, "DICOM files in folders")),
      h("a", { class: "menu-item", href: `/api/${kind}/${id}/download?dicomdir=true`, onclick: closeMenu }, "ZIP with DICOMDIR", h("small", null, "Adds an index, like a CD/DVD"))));
}

/**
 * Dialog to send a whole study to a remote DICOM node (an Orthanc "modality").
 * The transfer runs as an Orthanc job; follow it in the Transfers tab.
 */
async function openSendDialog(study) {
  let nodes;
  try {
    nodes = await api("GET", "/api/modalities");
  } catch (e) { toastError(e); return; }
  if (!nodes.length) {
    toast("No DICOM destination is configured in Orthanc (DicomModalities).", "error", 8000);
    return;
  }

  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const target = h("select", null, nodes.map((n) =>
    h("option", { value: n.id }, [n.description ? `${n.description}: ${n.id}` : n.id, n.aet ? `  (${n.aet} at ${n.host}:${n.port})` : ""].join(""))));
  const submit = h("button", { class: "btn primary", type: "submit" }, "Send study");

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      submit.disabled = true;
      try {
        await api("POST", `/api/studies/${study.id}/send`, { target: target.value });
        dialog.close();
        toast(`Transfer to ${target.value} started. Follow it in the Transfers tab.`, "info", 7000);
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
      }
    },
  },
    h("h2", null, "Send study to a DICOM destination"),
    h("p", null, h("b", null, formatName(study.patient_name)), " ", formatDicomDate(study.study_date), study.study_description ? `, ${study.study_description}` : ""),
    error,
    h("label", null, "Destination", target),
    h("p", { class: "muted" }, "Orthanc sends every image of the study to the chosen node using DICOM C-STORE."),
    h("div", { class: "dialog-actions" },
      h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"),
      submit)));

  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

// Clicking anywhere else closes any open download menu.
document.addEventListener("click", (event) => {
  document.querySelectorAll("details.menu[open]").forEach((d) => { if (!d.contains(event.target)) d.removeAttribute("open"); });
});

/* ====================================================================
   8. UPLOAD
   ==================================================================== */

/**
 * Upload progress lives at module level so it keeps running when the user
 * switches tabs. Each file is sent in its own request (see src/routes/upload.rs).
 */
const upload = {
  pending: [],            // files waiting to be sent (those before `next` are already taken)
  next: 0,                // index of the next file to send (cheaper than shift() on a big array)
  inFlight: new Map(),    // File -> { file, loaded } currently being sent
  total: 0, uploaded: 0, duplicates: 0, failed: 0,
  studyIds: new Set(),    // Orthanc ids of the studies that received files
  errors: [],             // [{ name, message }] (capped)
  finished: false,
  ui: null,               // references to the DOM nodes of the visible Upload tab
  // Every upload (one drop or one pick of files / folder) of this page session, newest first,
  // for the list on the Uploads page. Kept in memory only: a reload starts a new list.
  batches: [],
  batchSeq: 0,
  historyDrawnAt: 0,      // when that list was last redrawn (see updateUploadUi)
};

/** "1.4 MB", "820 KB", "3.2 GB". */
function formatBytes(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes, unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** A name for an upload: the dropped folder or file, or "12 files". */
function describeUpload(files) {
  const folder = (files[0].webkitRelativePath || "").split("/")[0];
  if (folder && files.every((f) => (f.webkitRelativePath || "").startsWith(`${folder}/`))) return `Folder ${folder}`;
  return files.length === 1 ? files[0].name : `${files.length} files`;
}

/** Starts a fresh batch (counters to zero) when nothing is running. */
function resetUploadStats() {
  Object.assign(upload, { total: 0, uploaded: 0, duplicates: 0, failed: 0, errors: [], finished: false });
  upload.studyIds = new Set();
}

/**
 * Adds files to the queue and starts sending. Each call is one "upload" in the list of the
 * Uploads page; `label` names it (the dropped folder...), otherwise it is worked out from the files.
 */
function enqueueFiles(fileList, label = "") {
  // Skip hidden/system files and DICOMDIR (an index, not an image).
  const files = Array.from(fileList).filter((f) => !f.name.startsWith(".") && f.name.toUpperCase() !== "DICOMDIR");
  if (!files.length) { toast("No uploadable files found.", "error"); return; }
  if (upload.next >= upload.pending.length && !upload.inFlight.size) resetUploadStats();
  const batch = {
    id: ++upload.batchSeq, label: label || describeUpload(files), started: Date.now(), finishedAt: 0,
    total: files.length, uploaded: 0, duplicates: 0, failed: 0, bytesTotal: 0, bytesDone: 0, studyIds: new Set(),
  };
  upload.batches.unshift(batch);
  if (upload.batches.length > 50) upload.batches.length = 50;
  // A loop, not push(...files): spreading a very large array (a whole archive folder)
  // exceeds the call-stack limit.
  for (const file of files) { upload.pending.push({ file, batch }); batch.bytesTotal += file.size; }
  upload.total += files.length;
  upload.finished = false;
  pump();
}

/** Keeps up to UPLOAD_CONCURRENCY requests running. */
function pump() {
  while (upload.inFlight.size < UPLOAD_CONCURRENCY && upload.next < upload.pending.length) {
    const { file, batch } = upload.pending[upload.next++];
    const item = { file, batch, loaded: 0 };
    upload.inFlight.set(item, item);
    sendFile(item).then(() => {
      upload.inFlight.delete(item);
      pump();
    });
  }
  if (upload.next >= upload.pending.length && upload.pending.length) { upload.pending = []; upload.next = 0; } // free the list
  scheduleUploadUi();
  if (upload.next >= upload.pending.length && !upload.inFlight.size && upload.total > 0 && !upload.finished) {
    upload.finished = true;
    updateUploadUi();
    // Simplified view: the new studies appear on the home page and in the total at once.
    if (upload.uploaded > 0) {
      if (reloadRecentStudies) reloadRecentStudies();
      refreshTotalStudies(true);
    }
    toast(`Upload finished: ${upload.uploaded} new, ${upload.duplicates} already stored, ${upload.failed} failed.`,
      upload.failed ? "error" : "info", 8000);
  }
}

function failFile(item, message) {
  upload.failed++;
  item.batch.failed++;
  if (upload.errors.length < 200) upload.errors.push({ name: item.file.webkitRelativePath || item.file.name, message });
}

/** A file of `item.batch` is done (sent, already there, or failed). */
function fileDone(item) {
  const b = item.batch;
  b.bytesDone += item.file.size;
  if (b.uploaded + b.duplicates + b.failed >= b.total) b.finishedAt = Date.now();
}

/** Sends ONE file as the raw request body. Always resolves (errors are counted, not thrown). */
function sendFile(item) {
  return new Promise((resolve) => {
    const xhr = new XMLHttpRequest(); // XHR (not fetch) because it reports upload progress
    xhr.open("POST", "/api/upload");
    xhr.setRequestHeader("X-Requested-With", "portal");
    xhr.setRequestHeader("Content-Type", "application/dicom");
    xhr.setRequestHeader("X-File-Name", encodeURIComponent(item.file.webkitRelativePath || item.file.name));
    xhr.upload.onprogress = (event) => { item.loaded = event.loaded; scheduleUploadUi(); };
    xhr.onload = () => {
      let data = null;
      try { data = JSON.parse(xhr.responseText); } catch { /* not JSON */ }
      const b = item.batch;
      if (xhr.status >= 200 && xhr.status < 300) {
        if (data && data.Status === "AlreadyStored") { upload.duplicates++; b.duplicates++; } else { upload.uploaded++; b.uploaded++; }
        if (data && data.ParentStudy) { upload.studyIds.add(data.ParentStudy); b.studyIds.add(data.ParentStudy); }
      } else {
        failFile(item, (data && data.error) || `HTTP ${xhr.status}`);
        if (xhr.status === 401) { upload.pending = []; upload.next = 0; sessionExpired(); } // stop: we are signed out
      }
      fileDone(item);
      resolve();
    };
    xhr.onerror = () => { failFile(item, "Network error"); fileDone(item); resolve(); };
    xhr.send(item.file);
  });
}

/**
 * Asks for a redraw of the upload panel, at most once per screen frame. Progress events
 * and file completions arrive far more often than a screen can show (thousands per
 * second for a big folder of small files): redrawing for each one would waste the CPU.
 */
let uploadUiQueued = false;
function scheduleUploadUi() {
  if (uploadUiQueued) return;
  uploadUiQueued = true;
  const redraw = () => { uploadUiQueued = false; updateUploadUi(); };
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(redraw); else setTimeout(redraw, 100);
}

/** Is something being uploaded right now? */
const uploading = () => upload.inFlight.size > 0 || upload.next < upload.pending.length;

/** Overall progress of the current upload queue, 0..1 (files done plus parts of the files being sent). */
function uploadFraction() {
  const done = upload.uploaded + upload.duplicates + upload.failed;
  let partial = 0;
  for (const item of upload.inFlight.values()) partial += item.file.size ? item.loaded / item.file.size : 0;
  return upload.total ? Math.min(1, (done + partial) / upload.total) : 0;
}

/**
 * The small "42%" on the Uploads icon / tab while files are being sent, so the upload can be
 * followed from any page. updateUploadUi() keeps it up to date.
 */
function uploadBadge() {
  const badge = h("span", { class: "upload-badge", id: "upload-badge", "aria-hidden": "true", hidden: !uploading() });
  badge.textContent = uploading() ? `${Math.floor(uploadFraction() * 100)}%` : ""; // empty when idle: the tab reads "Uploads"
  return badge;
}

/** Progress of one upload of the list: bytes sent so far, including the files being sent. */
function batchSent(b) {
  let sent = b.bytesDone;
  for (const item of upload.inFlight.values()) if (item.batch === b) sent += item.loaded;
  return Math.min(sent, b.bytesTotal);
}

/** The list of the Uploads page: every upload of this session, newest first. */
function renderUploadHistory() {
  if (!upload.batches.length) return h("div", { class: "empty" }, "Nothing uploaded yet. Uploads of this browser session are listed here with their progress.");
  const now = Date.now();
  const rows = upload.batches.map((b) => {
    const done = b.uploaded + b.duplicates + b.failed;
    const sent = batchSent(b);
    const fraction = b.bytesTotal ? sent / b.bytesTotal : (b.total ? done / b.total : 1);
    const running = !b.finishedAt;
    const started = running && (done > 0 || sent > 0);
    const seconds = ((b.finishedAt || now) - b.started) / 1000;
    let detail;
    if (!running) detail = `${formatBytes(b.bytesTotal)} in ${formatDuration(Math.max(1, seconds))}`;
    else if (!started) detail = "Waiting for the uploads before it";
    else {
      const speed = sent / Math.max(seconds, 0.5);   // bytes per second since it started
      const left = speed > 0 ? (b.bytesTotal - sent) / speed : 0;
      detail = `${formatBytes(sent)} of ${formatBytes(b.bytesTotal)} · ${formatBytes(speed)}/s · about ${formatDuration(Math.max(1, left))} left`;
    }
    const fill = h("span");
    fill.style.width = `${Math.round(fraction * 100)}%`;
    const [stateClass, stateText] = running ? (started ? ["state-running", "Uploading"] : ["state-pending", "Waiting"])
      : b.failed ? ["state-failure", b.failed === b.total ? "Failed" : "Done with errors"] : ["state-success", "Done"];
    return h("tr", null,
      h("td", { class: "nowrap" }, new Date(b.started).toLocaleTimeString()),
      h("td", null, h("div", { class: "patient" }, b.label), h("div", { class: "sub" }, `${b.total} ${b.total === 1 ? "file" : "files"}, ${formatBytes(b.bytesTotal)}`)),
      h("td", { class: "upload-progress-cell" },
        h("div", { class: "upload-progress-line" }, h("div", { class: "progress" }, fill), h("b", null, `${Math.floor(fraction * 100)}%`)),
        h("div", { class: "sub" }, detail)),
      h("td", { class: "num nowrap" }, `${done} of ${b.total}`),
      h("td", { class: "num" }, b.uploaded),
      h("td", { class: "num" }, b.duplicates),
      h("td", { class: b.failed ? "num error-line" : "num" }, b.failed),
      h("td", { class: "num" }, b.studyIds.size),
      h("td", null, h("span", { class: `chip ${stateClass}` }, stateText)));
  });
  return h("div", { class: "table-wrap" }, h("table", { class: "data" },
    h("thead", null, h("tr", null, ["Started", "Upload", "Progress", "Files", "New", "Already stored", "Failed", "Studies", "State"]
      .map((t) => h("th", { class: ["Files", "New", "Already stored", "Failed", "Studies"].includes(t) ? "num" : null }, t)))),
    h("tbody", null, rows)));
}

/** Redraws counters, progress bar, active files and error list (if the tab is visible). */
function updateUploadUi() {
  const badge = document.getElementById("upload-badge");
  if (badge) {
    badge.hidden = !uploading();
    badge.textContent = uploading() ? `${Math.floor(uploadFraction() * 100)}%` : "";
  }
  // The list of the Uploads page (it is there even when nothing is running). Redrawn at most twice a
  // second: progress events come far more often, and the list can hold 50 uploads. The last redraw,
  // when everything is done, is never skipped.
  const history = upload.ui && upload.ui.history;
  if (history && history.isConnected && (!uploading() || Date.now() - upload.historyDrawnAt > 500)) {
    upload.historyDrawnAt = Date.now();
    history.replaceChildren(renderUploadHistory());
  }
  const ui = upload.ui;
  if (!ui || !document.body.contains(ui.root)) return;

  const done = upload.uploaded + upload.duplicates + upload.failed;
  let partial = 0;
  for (const item of upload.inFlight.values()) partial += item.file.size ? item.loaded / item.file.size : 0;
  ui.bar.style.width = upload.total ? `${Math.min(100, ((done + partial) / upload.total) * 100)}%` : "0%";

  ui.statDone.textContent = `${done} of ${upload.total}`;
  ui.statNew.textContent = upload.uploaded;
  ui.statDup.textContent = upload.duplicates;
  ui.statFail.textContent = upload.failed;
  ui.statStudies.textContent = upload.studyIds.size;
  ui.root.hidden = upload.total === 0;
  ui.stats.hidden = upload.total === 0;
  ui.barWrap.hidden = upload.total === 0;

  // One small progress row per file being sent right now. The bar width is set
  // through the DOM (`style.width`), which the Content-Security-Policy allows,
  // unlike inline style="" attributes written in markup.
  ui.active.replaceChildren(...Array.from(upload.inFlight.values()).map((item) => {
    const fill = h("span");
    fill.style.width = item.file.size ? `${(item.loaded / item.file.size) * 100}%` : "0%";
    return h("div", { class: "row" },
      h("span", { class: "name mono" }, item.file.webkitRelativePath || item.file.name),
      h("div", { class: "progress" }, fill));
  }));

  ui.errors.replaceChildren(...upload.errors.map((e) => h("div", { class: "error-line" }, `${e.name}: ${e.message}`)));
  ui.viewButton.hidden = !(upload.finished && upload.uploaded + upload.duplicates > 0);
}

/**
 * Collects the files of a drag & drop, descending into dropped folders.
 * (`dataTransfer.items` must be read synchronously, before the first `await`.)
 */
async function filesFromDrop(dataTransfer) {
  const entries = Array.from(dataTransfer.items || [])
    .map((item) => (item.webkitGetAsEntry ? item.webkitGetAsEntry() : null))
    .filter(Boolean);
  if (!entries.length) return Array.from(dataTransfer.files);
  // A name for the list of the Uploads page: "Folder CT_2024", "scan.dcm" or "3 items".
  const label = entries.length === 1 ? (entries[0].isDirectory ? `Folder ${entries[0].name}` : entries[0].name) : `${entries.length} items`;

  const files = [];
  async function walk(entry) {
    if (entry.isFile) {
      files.push(await new Promise((resolve, reject) => entry.file(resolve, reject)));
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      for (;;) { // readEntries returns results in batches until it returns an empty one
        const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
        if (!batch.length) break;
        for (const child of batch) await walk(child);
      }
    }
  }
  for (const entry of entries) await walk(entry);
  files.label = label;
  return files;
}

/**
 * An area that accepts dropped files and folders, with buttons to pick them instead.
 * `home` = the compact version of the simplified view's home page.
 */
function buildDropzone({ title, hint = "", home = false }) {
  const onPicked = (event) => { enqueueFiles(event.target.files); event.target.value = ""; };
  const fileInput = h("input", { type: "file", multiple: true, hidden: true, onchange: onPicked });
  const folderInput = h("input", { type: "file", multiple: true, webkitdirectory: true, hidden: true, onchange: onPicked });

  const dropzone = h("div", { class: home ? "dropzone home-drop" : "dropzone" },
    h("h2", null, title),
    hint ? h("p", { class: "muted" }, hint) : null,
    h("div", { class: "buttons" },
      h("button", { class: home ? "btn small" : "btn primary", type: "button", onclick: () => fileInput.click() }, home ? "or choose files" : "Choose files"),
      h("button", { class: home ? "btn small" : "btn", type: "button", onclick: () => folderInput.click() }, home ? "or a folder" : "Choose folder")),
    fileInput, folderInput);

  dropzone.addEventListener("dragover", (e) => { e.preventDefault(); dropzone.classList.add("over"); });
  dropzone.addEventListener("dragleave", () => dropzone.classList.remove("over"));
  dropzone.addEventListener("drop", async (e) => {
    e.preventDefault();
    dropzone.classList.remove("over");
    const files = await filesFromDrop(e.dataTransfer);
    enqueueFiles(files, files.label);
  });
  return dropzone;
}

/** The "Uploads" tab: the drop zone, the progress of the current upload and every upload of this session. */
function buildUploadView() {
  const dropzone = buildDropzone({
    title: "Drop DICOM files or a whole study folder here",
    hint: "DICOM files (.dcm or without extension) and ZIP archives are accepted. Each file is sent separately.",
  });
  const progress = buildUploadProgress();
  const history = h("section", { class: "panel" });
  upload.ui.history = history;
  const view = h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Uploads")), dropzone, progress,
    h("div", { class: "page-head" }, h("h2", null, "Uploads of this session")), history);
  history.replaceChildren(renderUploadHistory());
  // While something is being sent the list refreshes with every progress event; this keeps the
  // speed and time left moving between them, and stops by itself when the page is left.
  const tick = setInterval(() => {
    if (!history.isConnected) { clearInterval(tick); return; }
    if (uploading()) { upload.historyDrawnAt = Date.now(); history.replaceChildren(renderUploadHistory()); }
  }, 1000);
  return view;
}

/**
 * The progress panel of the uploads (bar, counters, files being sent, errors). It registers
 * itself as THE panel updateUploadUi() draws into; only one is on screen at a time.
 */
function buildUploadProgress() {
  // Progress area (hidden until a file is queued).
  const bar = h("span");
  const barWrap = h("div", { class: "progress", hidden: true }, bar);
  const stat = (label) => { const value = h("b", null, "0"); return [value, h("div", { class: "stat" }, value, label)]; };
  const [statDone, doneBox] = stat("files processed");
  const [statNew, newBox] = stat("new");
  const [statDup, dupBox] = stat("already stored");
  const [statFail, failBox] = stat("failed");
  const [statStudies, studiesBox] = stat("studies touched");
  const stats = h("div", { class: "upload-stats", hidden: true }, doneBox, newBox, dupBox, failBox, studiesBox);
  const active = h("div", { class: "upload-active" });
  const errors = h("div", { class: "upload-errors" });
  const viewButton = h("button", {
    class: "btn primary", type: "button", hidden: true,
    onclick: openStudies,
  }, "Browse studies");

  const panel = h("section", { class: "panel", hidden: true }, barWrap, stats, active, errors, h("div", { class: "upload-errors" }, viewButton));
  upload.ui = { root: panel, bar, barWrap, stats, statDone, statNew, statDup, statFail, statStudies, active, errors, viewButton };
  updateUploadUi();
  return panel;
}

/* ====================================================================
   9. USERS (ADMIN) AND AUDIT LOG
   ==================================================================== */

/**
 * The "Transfers" tab: studies sent to remote DICOM nodes and the live state of
 * each Orthanc job. It refreshes by itself every 3 seconds while a transfer is
 * still pending or running, and stops as soon as you leave the tab.
 */
function buildTransfersView() {
  const container = h("section", { class: "panel" });
  // The destinations, to name them (description, AE title, address): asked once per visit.
  let nodes = new Map();
  const nodesLoaded = api("GET", "/api/modalities").then((list) => { nodes = new Map(list.map((n) => [n.id, n])); }).catch(() => {});

  async function load() {
    try {
      await nodesLoaded;
      const rows = await api("GET", "/api/transfers");
      container.replaceChildren(rows.length ? renderTransfers(rows) : h("div", { class: "empty" }, 'Nothing sent yet. Use "Send to..." on a study.'));
      // Keep polling only while something is still in progress and this tab is visible.
      if (rows.some((r) => ["Pending", "Running", "Retry"].includes(r.state))) {
        setTimeout(() => { if (document.body.contains(container)) load(); }, 3000);
      }
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  /** The destination: its short name, with its description and address in a tooltip and under it. */
  function destinationCell(alias) {
    const n = nodes.get(alias);
    const address = n && n.aet ? `AE title ${n.aet} at ${n.host}:${n.port}` : "";
    const tip = n
      ? [n.description, address].filter(Boolean).join(" — ") || alias
      : `${alias}: no longer among the DICOM destinations`;
    return h("td", null,
      h("span", { class: "chip has-tip", "data-tip": tip, "aria-label": `${alias}: ${tip}`, tabindex: "0" }, alias),
      n && n.description ? h("div", { class: "sub" }, n.description) : null);
  }

  /** "45%, 54 of 120 images, 1 min 10 s, about 1 min 25 s left" while it runs; the outcome when finished. */
  function progressCell(r) {
    const fill = h("span");
    fill.style.width = `${r.progress}%`;
    const images = r.instances;
    const ran = r.runtime ? formatDuration(r.runtime) : "";
    let detail;
    if (r.state === "Pending") detail = "Waiting to start";
    else if (r.state === "Running" || r.state === "Retry") {
      const parts = [];
      if (images) parts.push(`${Math.round((images * r.progress) / 100)} of ${images} images`);
      if (ran) parts.push(ran);
      if (r.runtime && r.progress > 0 && r.progress < 100) parts.push(`about ${formatDuration((r.runtime * (100 - r.progress)) / r.progress)} left`);
      detail = parts.join(" · ") || "Sending";
    } else if (r.state === "Success") detail = images ? `${images} images in ${ran || "a moment"}` : "Sent";
    else if (r.state === "Failure") detail = images && r.failed ? `${r.failed} of ${images} images could not be sent` : "";
    else if (r.state === "Unknown") detail = "Orthanc no longer remembers this transfer";
    else if (r.state === "Unavailable") detail = "Orthanc did not answer: trying again";
    const running = ["Pending", "Running", "Retry"].includes(r.state);
    return h("td", { class: "transfer-progress" },
      running ? h("div", { class: "upload-progress-line" }, h("div", { class: "progress" }, fill), h("b", null, `${r.progress}%`)) : null,
      detail ? h("div", { class: "sub" }, detail) : null,
      r.error ? h("div", { class: "error-line" }, r.error) : null);
  }

  function renderTransfers(rows) {
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Started", "Study", "Destination", "By", "Progress", "State"].map((t) => h("th", null, t)))),
      h("tbody", null, rows.map((r) => h("tr", null,
        h("td", { class: "nowrap" }, formatTimestamp(r.ts)),
        h("td", null, r.label.replaceAll(" | ", " - ")),
        destinationCell(r.target),
        h("td", null, r.username),
        progressCell(r),
        h("td", null, h("span", { class: `chip state-${r.state.toLowerCase()}` }, r.state)))))));
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Transfers"), actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load })),
    container);
}

/** The "Users" tab: list of accounts with edit / delete. */
function buildUsersView() {
  const container = h("section", { class: "panel" });
  let meta = null; // { users, permissions, presets }

  async function load() {
    container.replaceChildren(h("div", { class: "loading" }, "Loading users..."));
    try {
      meta = await api("GET", "/api/admin/users");
      container.replaceChildren(renderUsersTable());
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  /** Name of a role key ("Custom" = permissions ticked by hand). */
  const roleLabel = (key) => {
    const role = key && meta.presets.find((p) => p.key === key);
    return role ? role.label : h("span", { class: "muted" }, "Custom");
  };

  function renderUsersTable() {
    return h("div", { class: "table-wrap" },
      h("table", { class: "data" },
        h("thead", null, h("tr", null, ["User", "Role", "Access", "Status", "Last sign-in", "Actions"].map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
        h("tbody", null, meta.users.map((u) => h("tr", null,
          h("td", null, h("span", { class: "patient" }, u.username), u.id === state.user.id ? h("span", { class: "sub" }, " (you)") : null),
          h("td", null, roleLabel(u.effective_role), u.interface === "simplified" ? h("div", { class: "sub" }, "simplified view") : null),
          h("td", null, u.permissions.length ? u.permissions.map((p) => h("span", { class: "chip perm" }, p)) : h("span", { class: "muted" }, "no access")),
          h("td", null, h("span", { class: `chip ${u.active ? "status-on" : "status-off"}` }, u.active ? "Active" : "Disabled")),
          h("td", null, formatTimestamp(u.last_login)),
          h("td", null, h("div", { class: "actions" },
            h("button", { class: "btn small", type: "button", onclick: () => openUserDialog(u, meta, load) }, "Edit"),
            h("button", {
              class: "btn small danger", type: "button", disabled: u.id === state.user.id,
              onclick: async () => {
                if (!confirm(`Delete the user "${u.username}"? This cannot be undone.`)) return;
                try { await api("DELETE", `/api/admin/users/${u.id}`); toast("User deleted."); load(); } catch (e) { toastError(e); }
              },
            }, "Delete"))))))));
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" },
      h("h2", null, "Users"),
      h("button", { class: "btn primary", type: "button", onclick: () => meta && openUserDialog(null, meta, load) }, "New user")),
    container);
}

/**
 * Modal form to create (`user` = null) or edit an account.
 * A role preset fills the permission checkboxes; they can still be adjusted by hand.
 */
function openUserDialog(user, meta, reload) {
  const isNew = !user;
  const error = h("div", { class: "form-error", role: "alert", hidden: true });

  const username = h("input", { name: "username", required: true, autocomplete: "off", value: user ? user.username : "", disabled: !isNew });
  const password = h("input", {
    name: "password", type: "password", autocomplete: "new-password", required: isNew, minlength: 10,
    placeholder: isNew ? "At least 10 characters" : "Leave blank to keep the current password",
  });

  // One checkbox per permission; new users start as "viewer".
  const checks = {};
  const permissionBoxes = meta.permissions.map((p) => {
    const box = h("input", { type: "checkbox", checked: user ? user.permissions.includes(p.key) : ["read", "visualize"].includes(p.key) });
    checks[p.key] = box;
    return h("label", null, box, h("span", null, p.key, h("small", null, p.description)));
  });

  // The role is saved with the account: it decides the user's view (normal or simplified) while
  // the simplified view is switched on. Choosing one also ticks its permissions.
  const initialRole = user ? user.effective_role || "" : (meta.presets.some((p) => p.key === "viewer") ? "viewer" : "");
  const preset = h("select", {
    onchange: (event) => {
      const chosen = meta.presets.find((p) => p.key === event.target.value);
      if (chosen) for (const key of Object.keys(checks)) checks[key].checked = chosen.permissions.includes(key);
    },
  }, h("option", { value: "", selected: !initialRole }, "Custom (no role)"),
  meta.presets.map((p) => h("option", { value: p.key, selected: p.key === initialRole }, p.interface === "simplified" ? `${p.label}  [simplified view]` : p.label)));

  const active = h("input", { type: "checkbox", checked: user ? user.active : true });

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      const permissions = Object.keys(checks).filter((key) => checks[key].checked);
      try {
        if (isNew) {
          await api("POST", "/api/admin/users", { username: username.value.trim(), password: password.value, permissions, role: preset.value });
        } else {
          const body = { permissions, active: active.checked, role: preset.value };
          if (password.value) body.password = password.value;
          await api("PUT", `/api/admin/users/${user.id}`, body);
        }
        dialog.close();
        toast("User saved.");
        if (user && user.id === state.user.id) { // editing yourself: refresh your own permissions
          state.user = await api("GET", "/api/me");
          renderApp();
        } else {
          reload();
        }
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
      }
    },
  },
    h("h2", null, isNew ? "New user" : `Edit ${user.username}`),
    error,
    h("label", null, "Username", username),
    h("label", null, isNew ? "Password" : "New password (optional)", password),
    h("label", null, "Role", preset),
    h("p", { class: "hint" }, "Choosing a role ticks its permissions (you can still change them). The role also sets the user's view, normal or simplified, while the simplified view is switched on in Settings > Appearance."),
    h("fieldset", { class: "perm-list" }, h("legend", null, "Permissions"), permissionBoxes),
    isNew ? null : h("label", { class: "perm-list" }, h("span", null, active, " Account is active")),
    h("div", { class: "dialog-actions" },
      h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"),
      h("button", { class: "btn primary", type: "submit" }, "Save user"))));

  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/** The "Audit log" tab: who did what, newest first. */
function buildAuditView() {
  const container = h("section", { class: "panel" });

  async function load() {
    container.replaceChildren(h("div", { class: "loading" }, "Loading..."));
    try {
      const data = await api("GET", "/api/admin/audit?limit=300");
      container.replaceChildren(data.entries.length
        ? h("div", { class: "table-wrap" }, h("table", { class: "data" },
            h("thead", null, h("tr", null, ["Time", "User", "Action", "Details"].map((t) => h("th", null, t)))),
            h("tbody", null, data.entries.map((e) => h("tr", null,
              h("td", { class: "nowrap" }, formatTimestamp(e.ts)),
              h("td", null, e.username),
              h("td", null, h("span", { class: "chip" }, e.action)),
              h("td", { class: "mono" }, e.detail))))))
        : h("div", { class: "empty" }, "Nothing recorded yet."));
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "Audit log"), h("button", { class: "btn", type: "button", onclick: load }, "Refresh")),
    container);
}

/* ====================================================================
   9a. SHARING STUDIES WITH PEOPLE OUTSIDE
   A staff member creates an invitation: a link plus a generated password that opens ONE
   study for someone outside (online viewing + a limited number of ZIP downloads, until the
   link expires). The password can be shown again from the Shares list while the link is active,
   unless an administrator switched that off (Settings > Share links).
   ==================================================================== */

/**
 * Copies text to the clipboard. `navigator.clipboard` only exists on secure pages (HTTPS or
 * localhost) and the intranet is often plain HTTP, so fall back to the old way.
 */
async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); return true; }
  } catch { /* use the fallback */ }
  const area = h("textarea", null, text);
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  let copied = false;
  try { copied = document.execCommand("copy"); } catch { copied = false; }
  area.remove();
  return copied;
}

/** A button that copies `getText()` and says so. */
function copyButton(label, getText, iconName = "") {
  const props = {
    class: "btn small", type: "button",
    onclick: async () => toast((await copyText(getText())) ? "Copied." : "Could not copy: select the text and copy it by hand.", "info", 2500),
  };
  // An icon in the simplified view when the caller gives one (the Shares list); text everywhere else.
  return iconName ? actionButton("button", iconName, label, label, props) : h("button", props, label);
}

/** "1 d 3 h 20 min" / "45 min" / "30 s". */
function formatDuration(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  const days = Math.floor(s / 86400), hours = Math.floor((s % 86400) / 3600), minutes = Math.floor((s % 3600) / 60);
  if (days > 0) return `${days} d ${hours} h ${minutes} min`;
  if (hours > 0) return `${hours} h ${minutes} min`;
  if (minutes > 0) return `${minutes} min`;
  return `${s} s`;
}

/** "2 days 5 hours 20 minutes" for a number of minutes (the parts that are not zero). */
function durationWords(totalMinutes) {
  const d = Math.floor(totalMinutes / 1440), hh = Math.floor((totalMinutes % 1440) / 60), mm = totalMinutes % 60;
  const part = (n, one, many) => (n ? `${n} ${n === 1 ? one : many}` : "");
  return [part(d, "day", "days"), part(hh, "hour", "hours"), part(mm, "minute", "minutes")].filter(Boolean).join(" ") || "0 minutes";
}

/**
 * How long a link works: three fields (days, hours, minutes) instead of a number of hours.
 * `.minutes` is the total; `.problem()` says what is wrong, or "" when it is fine.
 */
function buildDurationInput(totalMinutes, maxHours, legend) {
  const maxMinutes = maxHours * 60;
  const number = (value, max, label) => h("input", { type: "number", min: 0, max, value, required: true, "aria-label": label, class: "duration-part" });
  const days = number(Math.floor(totalMinutes / 1440), Math.floor(maxMinutes / 1440), "Days");
  const hours = number(Math.floor((totalMinutes % 1440) / 60), 23, "Hours");
  const minutes = number(totalMinutes % 60, 59, "Minutes");
  const part = (input, text) => h("label", { class: "duration-field" }, input, h("span", null, text));
  const total = () => (Number(days.value) || 0) * 1440 + (Number(hours.value) || 0) * 60 + (Number(minutes.value) || 0);
  return {
    node: h("fieldset", { class: "duration" }, h("legend", null, `${legend} (at most ${durationWords(maxMinutes)})`),
      part(days, "days"), part(hours, "hours"), part(minutes, "minutes")),
    get minutes() { return total(); },
    problem() {
      if (total() < 1) return "Choose how long the link works: at least 1 minute.";
      if (total() > maxMinutes) return `A link can work for at most ${durationWords(maxMinutes)}.`;
      return "";
    },
  };
}

/**
 * "Send the invitation by email" of the Share windows: off by default. `.value` is null, or the
 * `email` object the server expects. `canIncludePassword()` says whether the portal has the password.
 */
function buildEmailOptions(canIncludePassword) {
  const feature = state.user.features.share;
  if (!feature.email) {
    return {
      node: h("p", { class: "hint" }, "To send invitations by email, an administrator must first set up the mail server in Settings > Network & sharing > Email."),
      value: null, refresh() {},
    };
  }
  const send = h("input", { type: "checkbox" });
  const to = h("input", { type: "text", placeholder: "name@example.com (several: separate them with commas)", autocomplete: "off", "aria-label": "Email addresses" });
  const message = h("textarea", { rows: 3, maxlength: 1000, placeholder: "Optional message, e.g. Here are the images we discussed on the phone." });
  const withPassword = h("input", { type: "checkbox" });
  const passwordHint = h("p", { class: "hint" });
  const details = h("div", { class: "email-details", hidden: true },
    h("label", null, "To", to),
    h("label", null, "Message (optional)", message),
    h("label", { class: "check" }, withPassword, "Include the password in the email"),
    passwordHint);
  function refresh() {
    details.hidden = !send.checked;
    to.required = send.checked;
    const possible = canIncludePassword();
    withPassword.disabled = !possible;
    if (!possible) withPassword.checked = false;
    passwordHint.textContent = !possible
      ? "This link's password is not kept, so it cannot go in the email: tick \"Give the link a new password\" to include one."
      : withPassword.checked
        ? "Anyone who can read this email can open the study. Safer: leave it out and give the password by phone or text message."
        : "The email says the password will be given separately (by phone or text message): safer.";
  }
  send.addEventListener("change", () => { refresh(); if (send.checked) to.focus(); });
  withPassword.addEventListener("change", refresh);
  refresh();
  return {
    node: h("div", { class: "email-options" }, h("label", { class: "check" }, send, "Send the invitation by email"), details),
    get value() {
      return send.checked ? { to: to.value, message: message.value, include_password: withPassword.checked } : null;
    },
    refresh,
  };
}

/** One line about the email sent with a link (answer of the server), or null. */
function emailOutcome(email) {
  if (!email) return null;
  return email.sent
    ? h("p", { class: "notice" }, `Invitation emailed to ${email.to}.`)
    : h("p", { class: "form-error" }, `The link is ready, but the email to ${email.to} was not sent: ${email.error}`);
}

/** Dialog to invite somebody outside to look at a study (the "Share..." button of a study). */
function openShareDialog(study) {
  const feature = state.user.features.share;
  const dialog = h("dialog", { class: "share-dialog" });
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  const close = h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close");
  const who = h("p", null, h("b", null, formatName(study.patient_name)), " ", formatDicomDate(study.study_date), study.study_description ? `, ${study.study_description}` : "");

  /** Step 1: how long and how many downloads. */
  function showForm() {
    if (!feature.configured) {
      dialog.replaceChildren(h("div", { class: "dialog-form" },
        h("h2", null, "Sharing is not set up yet"),
        h("p", null, "An administrator must first enter the public address of the share links in Settings, section Share links."),
        h("div", { class: "dialog-actions" },
          can("admin") ? h("button", { class: "btn primary", type: "button", onclick: () => { dialog.close(); openSettings("network"); } }, "Open settings") : null,
          close)));
      return;
    }
    const error = h("div", { class: "form-error", role: "alert", hidden: true });
    const duration = buildDurationInput(feature.default_hours * 60, feature.max_hours, "The link works for");
    const downloads = h("input", { type: "number", min: 0, max: feature.max_downloads, value: feature.default_downloads, required: true });
    const emailOptions = buildEmailOptions(() => true);
    const note = h("input", { maxlength: 120, placeholder: "For your own records, e.g. Dr. Smith, second opinion", autocomplete: "off" });
    const submit = h("button", { class: "btn primary", type: "submit" }, "Create link and password");
    dialog.replaceChildren(h("form", {
      class: "dialog-form",
      onsubmit: async (event) => {
        event.preventDefault();
        error.hidden = true;
        const problem = duration.problem();
        if (problem) { error.textContent = problem; error.hidden = false; return; }
        submit.disabled = true;
        try {
          showResult(await api("POST", `/api/studies/${study.id}/share`, {
            minutes: duration.minutes, downloads: Number(downloads.value), note: note.value, email: emailOptions.value,
          }));
        } catch (e) {
          error.textContent = e.message;
          error.hidden = false;
          submit.disabled = false;
        }
      },
    },
      h("h2", null, "Share this study with somebody outside"),
      who, error,
      duration.node,
      h("label", null, `Downloads allowed as ZIP (0 = online viewing only, up to ${feature.max_downloads})`, downloads),
      h("label", null, "Note (optional)", note),
      emailOptions.node,
      h("p", { class: "muted" }, "The person gets a link and a password that opens only this study. They can look at it online until the link expires; running out of downloads does not stop that."),
      h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), submit)));
  }

  /** Step 2: the link and the password. */
  function showResult(result) {
    const expires = new Date(result.expires_at * 1000).toLocaleString();
    const link = h("input", { readonly: true, value: result.url, "aria-label": "Link", onfocus: (e) => e.target.select() });
    const invitation = () => `You have been invited to look at an imaging study.\nLink: ${result.url}\nThe link works until ${expires}.\nThe password is sent to you separately.`;
    dialog.replaceChildren(h("div", { class: "dialog-form" },
      h("h2", null, "Link and password created"),
      who,
      h("label", null, "Link", h("div", { class: "secret-box" }, link, copyButton("Copy link", () => result.url))),
      h("div", null, h("div", { class: "muted" }, "Password"),
        h("div", { class: "secret-box" }, h("span", { class: "password" }, result.password), copyButton("Copy password", () => result.password))),
      result.password_kept
        ? h("p", { class: "muted" }, "Give the password to the person by a different channel than the link (a phone call, an SMS). You can see it again in the Shares list while the link is active.")
        : h("p", { class: "form-error" }, "The password is shown only now and cannot be recovered. Give it to the person by a different channel than the link (a phone call, an SMS)."),
      h("p", { class: "muted" }, `Valid until ${expires}. Downloads allowed: ${result.max_downloads}.`),
      emailOutcome(result.email),
      h("div", { class: "dialog-actions" }, copyButton("Copy invitation text (without the password)", invitation), close)));
  }

  showForm();
  dialog.showModal();
}

/**
 * Shows the password of an active invitation again (Shares list). The server records it in the
 * audit log; the password stays only in this dialog and is gone when it closes.
 */
async function showSharePassword(share) {
  let password;
  try {
    ({ password } = await api("POST", `/api/shares/${share.id}/password`));
  } catch (e) { toastError(e); return; }
  passwordDialog(share, "Password of the link", password,
    "Give it by a different channel than the link (a phone call, an SMS). Showing it was recorded in the audit log.");
}

/** A small dialog with a link's password and a copy button. */
function passwordDialog(share, title, password, message) {
  const dialog = h("dialog", null, h("div", { class: "dialog-form" },
    h("h2", null, title),
    h("p", null, share.label.replaceAll(" | ", " - "), share.note ? h("span", { class: "muted" }, ` (${share.note})`) : null),
    h("div", { class: "secret-box" }, h("span", { class: "password" }, password), copyButton("Copy password", () => password)),
    h("p", { class: "muted" }, message),
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/** Small modal with a question and two buttons; resolves true when confirmed. */
function confirmDialog(title, text, confirmLabel) {
  return new Promise((resolve) => {
    let answer = false;
    const dialog = h("dialog", null, h("div", { class: "dialog-form" },
      h("h2", null, title), h("p", null, text),
      h("div", { class: "dialog-actions" },
        h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"),
        h("button", { class: "btn primary", type: "button", onclick: () => { answer = true; dialog.close(); } }, confirmLabel))));
    dialog.addEventListener("close", () => { dialog.remove(); resolve(answer); });
    document.body.append(dialog);
    dialog.showModal();
  });
}

/** Gives a link a new password (and shows it). The old one stops working at once. */
async function newSharePassword(share, reload) {
  try {
    const r = await api("PUT", `/api/shares/${share.id}`, { new_password: true });
    passwordDialog(share, "New password of the link", r.password, r.password_kept
      ? "The previous password no longer works. Give this one by a different channel than the link (a phone call, an SMS); you can see it again later from the Shares list."
      : "The previous password no longer works. This one is shown only now: give it by a different channel than the link (a phone call, an SMS).");
    reload();
  } catch (e) { toastError(e); }
}

/** A link whose password was not kept (made before passwords were kept): offer a new one. */
async function offerNewSharePassword(share, reload) {
  const ok = await confirmDialog("This password cannot be shown",
    "This link was created before passwords were kept, so its password cannot be read back. You can give the link a new password: the current one stops working, and whoever is using the link must type the new one.",
    "Create a new password");
  if (ok) newSharePassword(share, reload);
}

/**
 * Changes a link (Shares > Edit): how long it still works (also brings an expired link back),
 * how many downloads are left, the note, and optionally a new password.
 */
function openShareEditor(share, reload) {
  const feature = state.user.features.share;
  const now = Date.now() / 1000;
  const active = share.status === "active";
  // The time left, to the minute (an expired link starts from the default validity).
  const initialMinutes = active ? Math.min(feature.max_hours * 60, Math.max(1, Math.ceil((share.expires_at - now) / 60))) : feature.default_hours * 60;
  const initialLeft = Math.min(feature.max_downloads, Math.max(0, share.max_downloads - share.downloads_used));
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const duration = buildDurationInput(initialMinutes, feature.max_hours, "The link works for, from now");
  const left = h("input", { type: "number", min: 0, max: feature.max_downloads, value: initialLeft, required: true });
  const note = h("input", { maxlength: 120, value: share.note || "", autocomplete: "off" });
  const newPassword = h("input", { type: "checkbox" });
  const emailOptions = buildEmailOptions(() => newPassword.checked || !!share.password_available || (!active && !!share.password_kept));
  newPassword.addEventListener("change", () => emailOptions.refresh());
  const submit = h("button", { class: "btn primary", type: "submit" }, "Save");

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      const problem = duration.problem();
      if (problem) { error.textContent = problem; error.hidden = false; return; }
      submit.disabled = true;
      // Only what was changed is sent: an untouched time left must not be rounded up to the minute.
      const body = { note: note.value };
      if (!active || duration.minutes !== initialMinutes) body.minutes = duration.minutes;
      if (Number(left.value) !== initialLeft) body.downloads_left = Number(left.value);
      if (newPassword.checked) body.new_password = true;
      if (emailOptions.value) body.email = emailOptions.value;
      try {
        const r = await api("PUT", `/api/shares/${share.id}`, body);
        dialog.close();
        reload();
        if (r.email) {
          if (r.email.sent) toast(`Invitation emailed to ${r.email.to}.`);
          else toast(`The link was updated, but the email was not sent: ${r.email.error}`, "error", 10000);
        }
        if (r.password) {
          passwordDialog(share, "New password of the link", r.password, r.password_kept
            ? "The previous password no longer works. Give this one by a different channel than the link; you can see it again later from the Shares list."
            : "The previous password no longer works. This one is shown only now: give it by a different channel than the link.");
        } else if (!r.email) {
          toast("Link updated.");
        }
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
      }
    },
  },
    h("h2", null, "Change the link"),
    h("p", null, h("b", null, share.label.replaceAll(" | ", " - "))),
    h("p", { class: "muted" }, active
      ? `It works for ${formatDuration(share.expires_at - now)} more (until ${formatTimestamp(share.expires_at)}). ${share.downloads_used} of ${share.max_downloads} downloads used.`
      : `It ended on ${formatTimestamp(share.expires_at)}: saving makes it work again. ${share.downloads_used} of ${share.max_downloads} downloads used.`),
    error,
    duration.node,
    h("label", null, `Downloads left from now on (0 = online viewing only, up to ${feature.max_downloads})`, left),
    h("label", null, "Note (for your own records)", note),
    h("label", { class: "check" }, newPassword, "Give the link a new password (the current one stops working)"),
    emailOptions.node,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), submit)));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/** The "Shares" tab: the invitations, with time and downloads left, and Copy link / Password / Edit / Revoke buttons. */
function buildSharesView() {
  const container = h("section", { class: "panel" });

  async function load() {
    container.replaceChildren(h("div", { class: "loading" }, "Loading..."));
    try {
      const shares = await api("GET", "/api/shares");
      container.replaceChildren(shares.length ? renderTable(shares) : h("div", { class: "empty" }, 'No invitation yet. Use "Share..." on a study.'));
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  function renderTable(shares) {
    const now = Date.now() / 1000;
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Study", "By", "Created", "Time left", "Downloads left", "Opened", "Status", "Actions"].map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
      h("tbody", null, shares.map((s) => h("tr", null,
        h("td", null, s.label.replaceAll(" | ", " - "), s.note ? h("div", { class: "sub" }, s.note) : null),
        h("td", null, s.created_by),
        h("td", { class: "nowrap" }, formatTimestamp(s.created_at)),
        timeLeftCell(s, now),
        downloadsCell(s),
        h("td", { class: "nowrap" }, s.access_count ? `${s.access_count}x, last ${formatTimestamp(s.last_access_at)}` : "never"),
        h("td", null, h("span", { class: `chip ${s.status === "active" ? "status-on" : "status-off"}` }, s.status)),
        h("td", null, h("div", { class: "actions" },
          s.url && s.status === "active" ? copyButton("Copy link", () => s.url, "link") : null,
          // Every active link: its kept password, or (made before passwords were kept) a new one.
          s.status === "active" && (s.password_available || s.password_kept)
            ? actionButton("button", "key", "Password", "Show the password", { class: "btn small", type: "button", onclick: () => (s.password_available ? showSharePassword(s) : offerNewSharePassword(s, load)) })
            : null,
          s.status !== "revoked"
            ? actionButton("button", "edit", "Edit...", "Change the time left, the downloads, the note or the password", { class: "btn small", type: "button", onclick: () => openShareEditor(s, load) })
            : null,
          s.status === "active"
            ? actionButton("button", "revoke", "Revoke", "Revoke this link", {
                class: "btn small danger", type: "button",
                onclick: async () => {
                  if (!confirm("Revoke this link? The person will not be able to open it any more.")) return;
                  try { await api("POST", `/api/shares/${s.id}/revoke`); toast("Link revoked."); load(); } catch (e) { toastError(e); }
                },
              })
            : null)))))));
  }

  /** How long the link still works, and until when (amber in its last hour). */
  function timeLeftCell(s, now) {
    const until = `until ${formatTimestamp(s.expires_at)}`;
    if (s.status === "revoked") return h("td", { class: "nowrap muted" }, "withdrawn");
    if (s.status !== "active") return h("td", { class: "nowrap" }, h("span", { class: "muted" }, "ended"), h("div", { class: "sub" }, formatTimestamp(s.expires_at)));
    const left = s.expires_at - now;
    return h("td", { class: "nowrap" }, h("span", { class: left < 3600 ? "soon" : null }, formatDuration(left)), h("div", { class: "sub" }, until));
  }

  /** Downloads still allowed, and how many were used (amber when none are left). */
  function downloadsCell(s) {
    if (!s.max_downloads) return h("td", { class: "nowrap" }, "view only", h("div", { class: "sub" }, "no download allowed"));
    // A withdrawn or expired link allows nothing any more: only say what was used.
    if (s.status !== "active") return h("td", { class: "nowrap muted" }, `${s.downloads_used} of ${s.max_downloads} used`);
    const left = Math.max(0, s.max_downloads - s.downloads_used);
    return h("td", { class: "nowrap" },
      h("span", { class: left === 0 ? "soon" : null }, left === 0 ? "none left" : `${left} left`),
      h("div", { class: "sub" }, `${s.downloads_used} of ${s.max_downloads} used`));
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Shares"), actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load })),
    h("p", { class: "muted" }, "Invitations for people outside: a link plus a password that opens one study. The password of an active link can be shown again (each time is recorded in the audit log); revoke a link if it was sent to the wrong person."),
    container);
}

/* ====================================================================
   9c. HELP
   Instructions for everyday users, written for people who are not administrators. The page is
   built for the person reading it: only the parts their permissions give access to, only the
   features switched on in this installation (Weasis, OHIF, email...), and in the words of the
   view they use (icons in the simplified view, tabs in the normal one).
   ==================================================================== */

/** A small inline copy of an icon, to show in the text which button is meant. */
const helpIcon = (name) => h("span", { class: "help-icon", "aria-hidden": "true" }, icon(name));
/** A button name as it appears on screen: the icon in the simplified view, the text in the normal one. */
const helpButton = (iconName, text) => (iconButtons() ? [helpIcon(iconName), ` (${text})`] : h("b", null, text));
/** A bullet list: one bullet per argument (an argument may be several pieces: text, icons...); null = left out. */
const ul = (...items) => h("ul", null, items.filter(Boolean).map((item) => h("li", null, item)));

/**
 * The sections of the Help page. `show` says whether this user sees it; `body` builds its content.
 * Keep the wording plain: the readers are clinical and front-desk staff, not IT people.
 */
function helpSections() {
  const simple = currentInterface() === "simplified";
  const f = state.user.features;
  const share = f.share || {};
  const where = (simpleName, normalName) => (simple ? h("b", null, simpleName) : h("b", null, normalName));
  return [
    {
      id: "around", title: "Getting around", show: true,
      body: () => [
        simple
          ? h("p", null, "The column of icons on the left takes you to each part of the portal (on a phone, the icons are along the bottom of the screen). Rest the mouse on an icon to see its name. Buttons that show only an icon work the same way: rest the mouse on one to see what it does.")
          : h("p", null, "The tabs at the top of the page take you to each part of the portal."),
        h("p", null, "What you can use:"),
        ul(
          simple ? [helpIcon("home"), " ", h("b", null, "Home"), ": the latest studies and an area to upload files."] : null,
          can("read") ? [simple ? [helpIcon("search"), " "] : null, where("Search", "Studies and Series"), ": find a study or a series."] : null,
          can("upload") ? [simple ? [helpIcon("upload"), " "] : null, h("b", null, "Uploads"), ": add images to the archive and follow the uploads."] : null,
          can("send") ? [simple ? [helpIcon("transfers"), " "] : null, h("b", null, "Transfers"), ": follow the studies you sent to another system."] : null,
          can("share") ? [simple ? [helpIcon("share"), " "] : null, h("b", null, "Shares"), ": the links you created to show a study to someone outside."] : null,
          [simple ? [helpIcon("help"), " "] : null, h("b", null, "Help"), ": this page."],
        ),
        h("p", { class: "muted" }, "You only see the parts your account gives access to. If you need something that is not here, ask your administrator."),
      ],
    },
    {
      id: "home", title: "The home page", show: simple,
      body: () => [
        h("p", null, `It lists the ${f.simplified && f.simplified.recent_count ? f.simplified.recent_count : ""} most recent studies, ${f.simplified && f.simplified.recent_order === "study_date" ? "the newest examination dates" : "the last ones received by the archive"} first. `,
          helpButton("refresh", "Refresh"), " updates the list."),
        can("read") ? h("p", null, "Each study has the same buttons as on the Search page (see below). The small arrow ▸ at the start of a row shows the series of the study.") : null,
        can("upload") ? h("p", null, "At the bottom, drag files or a whole folder onto the upload area to add them to the archive (see Uploading images).") : null,
      ],
    },
    {
      id: "search", title: "Finding a study", show: can("read"),
      body: () => [
        h("p", null, "Fill in one or more boxes, then press ", h("b", null, "Search studies"), ". Leave a box empty to ignore it."),
        ul(
          [h("b", null, "Patient name"), ": part of the name is enough (DOE finds DOE, JOHN). A star works as a wildcard: SMI* finds every name starting with SMI."],
          [h("b", null, "Patient ID"), " and ", h("b", null, "Study description"), ": also a part is enough."],
          [h("b", null, "Modality"), ": click the buttons (CT, MR, US...) to show only those examinations; you can choose several. Click again to remove one. Other codes can be typed in the box next to them."],
          [h("b", null, "Study date from / to"), ": a period; fill in only one of the two for \"since\" or \"until\"."],
        ),
        h("p", null, "The results show the newest studies first, 25 per page (", h("b", null, "Previous"), " / ", h("b", null, "Next"), " at the bottom). ",
          h("b", null, "Clear filters"), " empties every box."),
        ul(
          "Click the small arrow ▸ at the start of a row to see the series of the study (each set of images) and how many images each has.",
          f.thumbnails && can("visualize") ? "The small picture is a preview of the study or series: it appears after a moment." : null,
          "Rest the mouse on a modality tag (CT, MR...) to see its full name.",
          [h("b", null, "Series"), simple ? " (the second tab of the Search page)" : " tab", ": the same search, one row per series, with a ", h("b", null, "Body part"), " box (CHEST, KNEE...)."],
        ),
      ],
    },
    {
      id: "view", title: "Looking at the images", show: can("visualize"),
      body: () => [
        hasOhif() || hasWeasis() ? h("p", null, "Each study has a button for each viewer:") : h("p", null, "No image viewer is switched on in this portal at the moment: ask your administrator."),
        ul(
          hasOhif() ? [helpButton("ohif", "OHIF"), ": opens the study in a new tab of your browser. Nothing to install; works on any computer of the practice."] : null,
          hasWeasis() ? [helpButton("weasis", "Weasis"), ": opens the study in the Weasis program, which must be installed on your computer. The first time, your browser asks whether it may open Weasis: allow it."] : null,
        ),
      ],
    },
    {
      id: "download", title: "Downloading a study", show: can("download"),
      body: () => [
        h("p", null, helpButton("download", "Download"), " (on a study, or on one series) offers two kinds of ZIP file:"),
        ul(
          [h("b", null, "ZIP"), ": the image files in folders. Choose this one in most cases."],
          [h("b", null, "ZIP with DICOMDIR"), ": the same with an index, like the CD or DVD of a scanner; some programs need it."],
        ),
        h("p", null, "Large studies take a while: your browser shows the download as it goes."),
        h("p", { class: "muted" }, "Downloaded files contain patient data: keep them in a safe place and delete them when you no longer need them."),
      ],
    },
    {
      id: "send", title: "Sending a study to another system", show: can("send"),
      body: () => [
        h("p", null, helpButton("send", "Send to..."), " sends a whole study to another DICOM system the practice works with (a workstation, another archive...). Choose the destination in the list and confirm."),
        h("p", null, "The transfer continues on its own, even if you leave the page. ",
          "Follow it on the ", simple ? [helpIcon("transfers"), " "] : null, h("b", null, "Transfers"), simple ? " page" : " tab",
          ": while it runs it shows the percentage, how many images were sent, how long it has been running and about how long is left (the list updates itself); then how many images were sent, or why it failed."),
        h("p", null, "Rest the mouse on the name of a destination (for example \"ris\") to see what it is and where it is."),
      ],
    },
    {
      id: "share", title: "Sharing a study with someone outside", show: can("share"),
      body: () => (!share.configured
        ? [h("p", null, "Sharing is not set up in this portal yet: ask your administrator.")]
        : [
          h("p", null, "You can let a person outside the practice (a referring vet or doctor, a specialist, an owner or patient) look at ONE study, without giving them an account. They get a link and a password; with them they can see the study in their browser and, if you allow it, download it."),
          h("p", null, h("b", null, "To share a study:")),
          h("ol", null,
            h("li", null, "Find the study and press ", helpButton("share", "Share..."), "."),
            h("li", null, `Choose how long the link works (in days, hours and minutes, at most ${durationWords((share.max_hours || 0) * 60)}) and how many times the study may be downloaded (0 = online viewing only). Add a note for yourself if you like (for example "Dr. Smith, second opinion"): the person does not see it.`),
            share.email ? h("li", null, "If you want, tick ", h("b", null, "Send the invitation by email"), " and type their email address and a message.") : null,
            h("li", null, "Press ", h("b", null, "Create link and password"), ". Copy the link and the password with the copy buttons.")),
          h("p", null, h("b", null, "Give the password by a different way than the link"), ": for example, email the link and say the password on the phone or send it by text message. ",
            share.email ? "The email invitation leaves the password out unless you tick \"Include the password in the email\"." : null),
          h("p", null, "The person can look at the study until the link expires, even when the downloads are used up."),
          h("p", null, h("b", null, "The Shares page"), " lists your links: the time left, the downloads left, when it was opened, and its state (active, expired, revoked). On each link:"),
          ul(
            [helpButton("link", "Copy link"), ": copies the link again."],
            [helpButton("key", "Password"), ": shows the password again, if your administrator allows it (only while the link works; it is recorded)."],
            [helpButton("edit", "Edit..."), ": gives the link more time (this also brings back an expired link), more downloads, a new note or a new password", share.email ? ", and can email the update." : "."],
            [helpButton("revoke", "Revoke"), ": stops the link at once, for example if it was sent to the wrong person. This cannot be undone: create a new link if needed."],
          ),
        ]),
    },
    {
      id: "upload", title: "Uploading images", show: can("upload"),
      body: () => [
        h("p", null, "To add images to the archive (from a CD, a USB stick, another program...):"),
        ul(
          [simple ? "Drag the files, or a whole folder, onto the upload area of the Home page or of " : "Drag the files, or a whole folder, onto the upload area of ", h("b", null, "Uploads"), "."],
          ["Or press ", h("b", null, "Choose files"), " / ", h("b", null, "Choose folder"), " to pick them on your computer."],
        ),
        h("p", null, "DICOM image files and ZIP archives are accepted. Images that are already in the archive are not copied twice (they are counted as \"already stored\")."),
        h("p", null, "The ", h("b", null, "Uploads"), " page shows each upload with its progress, speed and time left, then how many images were new, already stored or failed. ",
          simple ? "While images are being sent, the Uploads icon shows the percentage, so you can do something else in the meantime." : "While images are being sent, the Uploads tab shows the percentage, so you can do something else in the meantime."),
        h("p", { class: "muted" }, "Do not close or reload the page while an upload is running: it would stop. When it is finished, the new studies appear in the search and on the home page."),
      ],
    },
    {
      id: "switch", title: "Switching view", show: !!f.interface_switch,
      body: () => [
        h("p", null, "The portal has two looks: the simplified view (icons on the left, recent studies on the home page) and the normal view (tabs at the top, more tools). ",
          simple ? [helpButton("swap", "Normal view"), " switches to the normal view."] : ["The ", h("b", null, "Simplified view"), " button at the top right switches to the simplified view."],
          " Your choice is remembered on this computer."),
      ],
    },
    {
      id: "session", title: "Signing out and good habits", show: true,
      body: () => [
        ul(
          [simple ? [helpButton("logout", "Log out")] : h("b", null, "Sign out"), " when you leave a shared computer: the portal shows patient data."],
          "After a while without signing in again, the portal asks for your password again. This is normal.",
          "After several wrong passwords in a row, the account is blocked for a few minutes: wait, then try again (or ask your administrator).",
          "Never give your password to anyone, including colleagues: ask your administrator to create an account for them.",
        ),
      ],
    },
    {
      id: "more", title: "Need more help?", show: true,
      body: () => [
        h("p", null, "Ask your administrator. They can give your account access to more parts of the portal, set up email or sharing, and help when a study cannot be found."),
        can("admin") ? h("p", { class: "muted" }, "As an administrator you also have Configuration (Settings), which is not described here: see the portal's README for setting it up.") : null,
      ],
    },
  ].filter((section) => section.show);
}

/** The Help page: a list of the sections this user sees, then the sections themselves. */
function buildHelpView() {
  const sections = helpSections();
  const blocks = sections.map((s) => h("section", { class: "panel help-section", id: `help-${s.id}` }, h("h2", null, s.title), s.body()));
  const toc = h("nav", { class: "help-toc", "aria-label": "Help topics" },
    sections.map((s) => h("button", {
      type: "button", class: "btn small",
      onclick: () => document.getElementById(`help-${s.id}`).scrollIntoView({ behavior: "smooth", block: "start" }),
    }, s.title)));
  return h("div", { class: "view help" },
    h("div", { class: "page-head" }, h("h1", null, "Help")),
    h("p", { class: "muted" }, `How to use ${state.brand.name}. This page describes what your account can do.`),
    toc, blocks);
}

/* ====================================================================
   9b. SETTINGS AND DICOM NODES
   Only administrators on the intranet see these tabs: the server does not even
   register their routes on the internet-facing port.
   ==================================================================== */

/** Shows or hides the red Orthanc notice under the top bar. */
function paintOrthancBanner() {
  const slot = document.getElementById("orthanc-banner");
  if (!slot) return;
  slot.replaceChildren();
  slot.hidden = !state.orthancProblem;
  if (state.orthancProblem) {
    slot.append(
      h("span", null, `Orthanc: ${state.orthancProblem} `),
      h("button", { class: "btn small", type: "button", onclick: () => openSettings("orthanc") }, "Open settings"));
  }
}

/** Tests the saved Orthanc connection (at most once a minute) and updates the notice. */
async function refreshOrthancStatus(force = false) {
  if (!can("admin")) return;
  if (!force && Date.now() - state.orthancCheckedAt < 60000) return;
  state.orthancCheckedAt = Date.now();
  try {
    const result = await api("POST", "/api/admin/orthanc/test", {});
    state.orthancProblem = result.ok ? "" : result.message;
  } catch { /* keep the previous state */ }
  paintOrthancBanner();
}

/**
 * The form, described as data. Each field becomes a labelled control; `key` is the
 * name of the setting on the server (see src/settings.rs).
 */
const SETTINGS_FIELDS = {
  portal: [
    { key: "portal_name", label: "Portal name", hint: "Shown in the top bar and on the login page." },
    { key: "browser_title", label: "Browser window title", placeholder: "Same as the portal name", hint: "The text of the browser tab and window. Leave empty to use the portal name." },
    { key: "show_portal_name", label: "Show the portal name next to the logo", type: "checkbox", wide: true, hint: "Untick it when your logo already contains the name." },
    { key: "login_notice", label: "Login notice", type: "textarea", wide: true, hint: "Optional text on the login page, for example: Authorised staff only." },
  ],
  orthanc: [
    { key: "orthanc_url", label: "Orthanc address", placeholder: "http://orthanc:8042", hint: "REST API of Orthanc, as seen from the machine running the portal." },
    { key: "orthanc_username", label: "Orthanc user name", hint: "The account the portal uses in Orthanc (one of the users in Orthanc's RegisteredUsers)." },
  ],
  network: [
    { key: "bind_addr", label: "Intranet address (the whole portal)", placeholder: "0.0.0.0:8080", hint: "IP:PORT. 0.0.0.0 = every network card." },
    { key: "remote_enabled", label: "Enable the internet listener (share links only)", type: "checkbox", wide: true },
    { key: "remote_bind_addr", label: "Internet address (share links only)", placeholder: "0.0.0.0:8081", hint: "Point the Cloudflare Tunnel here. Only invitation links work through it: there are no accounts and no search on this side." },
    { key: "public_base_url", label: "Public address for Weasis (optional)", placeholder: "https://pacs.example.com", hint: "Empty = automatic: the address you used to open the portal. (Weasis is for staff on the intranet; guests only get the browser viewer.)" },
  ],
  email: [
    { key: "smtp_host", label: "Mail server", placeholder: "smtp.example.com", hint: "Your practice's outgoing mail server (SMTP). Empty = the portal sends no email." },
    { key: "smtp_port", label: "Port", type: "number", min: 1, max: 65535, hint: "Usually 587 (STARTTLS), 465 (TLS) or 25 (a relay on your network)." },
    { key: "smtp_security", label: "Encryption", type: "select", options: [["starttls", "STARTTLS (usually port 587)"], ["tls", "TLS (usually port 465)"], ["none", "None (only for a relay on your own network)"]] },
    { key: "smtp_username", label: "User name on the mail server", hint: "Empty when the server accepts the portal without signing in (a relay on your network)." },
    { key: "smtp_from", label: "Sender of the emails", placeholder: "Chalklands PACS <pacs@example.com>", hint: "The address the invitations come from. Your mail server must allow the portal to send as this address." },
    { key: "email_footer", label: "Text at the end of every email", type: "textarea", wide: true, hint: "Optional: the practice's address, phone number, opening hours..." },
  ],
  sharing: [
    { key: "share_public_url", label: "Public address of the share links", placeholder: "https://pacs.example.com", wide: true, hint: "The address outsiders use to reach the internet listener (your Cloudflare hostname). Links look like <this address>/s/<code>. Sharing stays switched off until this is set." },
    { key: "share_default_hours", label: "Link validity, default (hours)", type: "number", min: 1, max: 8760, hint: "48 hours = two days." },
    { key: "share_max_hours", label: "Link validity, maximum (hours)", type: "number", min: 1, max: 8760, hint: "Nobody can create a longer link." },
    { key: "share_default_downloads", label: "Downloads allowed, default", type: "number", min: 0, max: 1000, hint: "0 = online viewing only. Running out of downloads never stops the online viewing: only the time does." },
    { key: "share_max_downloads", label: "Downloads allowed, maximum", type: "number", min: 0, max: 1000 },
    { key: "share_keep_passwords", label: "Let staff see a link's password again from the Shares list", type: "checkbox", wide: true,
      hint: "Only the person who created the link, or an administrator, and only while the link is active; every time is recorded in the audit log. A copy of the password is kept in the portal's database until the link is revoked, or 30 days after it expires (so it can still be extended with the same password). Untick it to show passwords only once, when the link is created (the copies kept so far are erased)." },
  ],
  security: [
    { key: "session_hours", label: "Session duration (hours)", type: "number", min: 1, max: 720, hint: "Applies to the next sign-ins." },
    { key: "weasis_token_minutes", label: "Weasis link validity (minutes)", type: "number", min: 5, max: 1440 },
    { key: "login_max_failures", label: "Failed sign-ins before lockout", type: "number", min: 3, max: 20 },
    { key: "login_lockout_minutes", label: "Lockout duration (minutes)", type: "number", min: 1, max: 1440 },
    { key: "audit_retention_days", label: "Keep the audit log for (days)", type: "number", min: 0, max: 3650, hint: "0 = keep everything. Older audit entries and finished transfers are deleted automatically." },
    { key: "cookie_secure", label: "Secure cookie", type: "select", options: [["auto", "Automatic (only over HTTPS)"], ["always", "Always"], ["never", "Never"]], hint: "Automatic suits the intranet over HTTP plus Cloudflare over HTTPS." },
  ],
  viewers: [
    { key: "weasis_enabled", label: "Show the Weasis button (desktop viewer)", type: "checkbox", wide: true,
      hint: ["Weasis needs to be ", h("a", { href: "https://weasis.org", target: "_blank", rel: "noopener" }, "installed"),
        " on each computer that uses it; the browser viewer (OHIF) works without installing anything."] },
    { key: "ohif", label: "Show the OHIF button (browser viewer)", type: "select", options: [["auto", "Automatic (when Orthanc has the OHIF plugin)"], ["on", "Always"], ["off", "Never"]], hint: "Opens the study in a new browser tab. Orthanc needs its OHIF plugin." },
    { key: "max_upload_mb", label: "Largest uploaded file (MB)", type: "number", min: 1, max: 102400, hint: "Applies to each file; a study is uploaded as many files." },
    { key: "thumbnails_enabled", label: "Show preview thumbnails next to studies and series", type: "checkbox", wide: true, hint: "Only staff with the visualize permission see them (guests with an invitation never do). Each one makes Orthanc decode one image." },
    { key: "thumbnail_cache_mb", label: "Thumbnail cache (MB)", type: "number", min: 0, max: 1024, hint: "Memory used to remember generated thumbnails. 0 = no cache." },
  ],
  simplified: [
    { key: "simplified_enabled", label: "Switch on the simplified view", type: "checkbox", wide: true,
      hint: "Off: everybody uses the normal view. On: each role uses the view chosen for it in Security & roles > Roles; users with custom permissions (no role) get the view below. Users with the switch_view permission get a button to change view themselves (their choice is remembered in their browser)." },
    { key: "default_interface", label: "View of users without a role", type: "select", options: [["normal", "Normal view"], ["simplified", "Simplified view"]], hint: "Users whose permissions were ticked by hand (\"Custom\" in the user form)." },
    { key: "simplified_recent_count", label: "Recent studies on the home page", type: "number", min: 1, max: 100 },
    { key: "simplified_recent_order", label: "Which studies are the most recent", type: "select", options: [["received", "The last ones received by the PACS"], ["study_date", "The newest study dates"]] },
    { key: "simplified_recent_layout", label: "How the recent studies are shown", type: "select", options: [["table", "Table, like the Studies page (same columns and buttons)"], ["cards", "Cards with big previews"]] },
    { key: "simplified_card_size", label: "Size of the study cards", type: "select", options: [["small", "Small (more per row)"], ["medium", "Medium"], ["large", "Large (bigger previews)"]], hint: "Only for the cards." },
    { key: "simplified_recent_title", label: "Title of the recent studies", placeholder: "Recent studies", maxlength: 60 },
    { key: "simplified_upload_text", label: "Text of the upload area", placeholder: "Drag a file to upload it", maxlength: 120 },
    { key: "simplified_show_total", label: "Show the total number of studies in the header", type: "checkbox", wide: true },
    { key: "simplified_show_labels", label: "Write the name under each icon", type: "checkbox", wide: true, hint: "Otherwise the name pops up when the mouse rests on an icon." },
  ],
};

/**
 * The colours of Settings > Appearance: key, label, hint and the colour the picker shows
 * while "Default" is ticked (the built-in light-theme colour).
 */
const COLOR_FIELDS = [
  { key: "color_accent", label: "Accent", fallback: "#2547c8", hint: "Main buttons, selected tabs and filters, links." },
  { key: "color_header", label: "Header", fallback: "#101a24", hint: "Top bar of the normal view and header of the simplified view. The text colour adapts itself." },
  { key: "color_sidebar", label: "Icon column", fallback: "#ffffff", hint: "Left column of the simplified view." },
  { key: "color_background", label: "Page background", fallback: "#f1f4f7", hint: "Behind the cards and tables. Pick a light colour for the light theme and a dark one for the dark theme." },
];

/**
 * Picker for one branding image (`kind` = "logo" or "favicon"): shows the current
 * image on a light and a dark background, uploads a new file and removes it.
 * Images are saved AS SOON AS they are chosen, independently of the "Save settings"
 * button, and the page is updated at once.
 */
function buildBrandImagePicker(kind, title, hint, accept) {
  const node = h("div", { class: "brand-picker setting wide" });
  const fileInput = h("input", { type: "file", accept, hidden: true });

  /** Draws the current state of this image. */
  function draw() {
    const current = kind === "logo" ? state.brand.logo : state.brand.favicon;
    const custom = !!(current && current.custom);
    const src = custom ? current.url : "/favicon.svg";
    const swatch = (cls) => h("div", { class: `swatch ${cls} ${kind}` }, h("img", { src, alt: "" }));
    node.replaceChildren(
      h("div", { class: "brand-picker-head" }, h("b", null, title), h("span", { class: "chip" }, custom ? "custom" : "default")),
      h("div", { class: "brand-previews" }, swatch("light"), swatch("dark")),
      h("div", { class: "filter-actions" },
        h("button", { class: "btn small", type: "button", onclick: () => fileInput.click() }, custom ? "Replace file" : "Choose file"),
        h("button", { class: "btn small danger", type: "button", disabled: !custom, onclick: remove }, "Remove (use the default)")),
      h("p", { class: "hint" }, hint),
      fileInput);
  }

  async function upload(file) {
    try {
      await api("PUT", `/api/admin/branding/${kind}`, file);
      await refreshBrand();
      toast(`${title} saved.`);
    } catch (e) { toastError(e); }
    draw();
  }
  async function remove() {
    try {
      await api("DELETE", `/api/admin/branding/${kind}`);
      await refreshBrand();
      toast(`${title} removed: the default is used again.`);
    } catch (e) { toastError(e); }
    draw();
  }

  fileInput.addEventListener("change", () => {
    const file = fileInput.files && fileInput.files[0];
    fileInput.value = "";
    if (file) upload(file);
  });
  draw();
  return node;
}

/**
 * Editor for the list of roles (shortcuts that pre-tick permissions in the user
 * form). `get()` returns the roles as the server expects them.
 */
function buildRolesEditor(roles, defaults, permissions) {
  const clone = (list) => list.map((r) => ({ key: r.key, label: r.label, permissions: [...r.permissions], interface: r.interface || "normal" }));
  let rows = clone(roles);
  const list = h("div", { class: "roles" });

  const draw = () => list.replaceChildren(...rows.map((role, index) => h("div", { class: "role-row" },
    h("input", { class: "role-key", value: role.key, placeholder: "key", "aria-label": "Role key", oninput: (e) => { role.key = e.target.value; } }),
    h("input", { class: "role-label", value: role.label, placeholder: "Name shown in the user form", "aria-label": "Role name", oninput: (e) => { role.label = e.target.value; } }),
    h("div", { class: "role-perms" }, permissions.map((p) => h("label", { class: "check", title: p.description },
      h("input", {
        type: "checkbox", checked: role.permissions.includes(p.key),
        onchange: (e) => { role.permissions = e.target.checked ? [...new Set([...role.permissions, p.key])] : role.permissions.filter((k) => k !== p.key); },
      }), p.key))),
    // The look of this role's users while the simplified view is switched on (Appearance tab).
    h("select", { class: "role-interface", "aria-label": "View", title: "View used by this role when the simplified view is switched on (Appearance tab)", onchange: (e) => { role.interface = e.target.value; } },
      h("option", { value: "normal", selected: role.interface !== "simplified" }, "Normal view"),
      h("option", { value: "simplified", selected: role.interface === "simplified" }, "Simplified view")),
    h("button", { class: "btn small danger", type: "button", onclick: () => { rows.splice(index, 1); draw(); } }, "Remove"))));
  draw();

  const node = h("div", { class: "wide" }, list,
    h("div", { class: "filter-actions" },
      h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ key: "", label: "", permissions: ["read", "visualize"], interface: "normal" }); draw(); } }, "Add role"),
      h("button", { class: "btn small", type: "button", onclick: () => { rows = clone(defaults); draw(); } }, "Restore default roles")));

  return { node, get: () => rows.map((r) => ({ key: r.key.trim(), label: r.label.trim(), permissions: r.permissions, interface: r.interface })) };
}

/**
 * Editor of the modality buttons of the search pages (Settings > General): order (move
 * up / down), shown or hidden, label, DICOM codes and description, plus add / remove /
 * restore defaults. A live preview shows the row exactly as users will see it, with the
 * "Other codes" box that is always there. Saved with the rest of the form.
 */
function buildModalityButtonsEditor(buttons, defaults) {
  const clone = (list) => list.map((b) => ({ label: b.label, codes: b.codes.join(", "), title: b.title || "", visible: b.visible !== false }));
  let rows = clone(buttons);
  const parseCodes = (text) => text.split(/[\s,;]+/).map((c) => c.toUpperCase()).filter(Boolean);
  const list = h("div", { class: "modbtn-list" });
  const preview = h("div", { class: "modbtn-preview" });

  function drawPreview() {
    const shown = rows.filter((r) => r.visible && r.label.trim()).map((r) => ({ label: r.label.trim(), codes: parseCodes(r.codes), title: r.title }));
    const picker = buildModalityPicker("", () => {}, shown);
    preview.replaceChildren(h("span", { class: "field-label" }, "Preview (what users see)"), picker.node);
  }

  /** Redraws the rows; `focus` = [row index, control name] to keep the keyboard where it was. */
  function draw(focus) {
    list.replaceChildren(...rows.map((row, index) => {
      const move = (step, name, symbol, label) => h("button", {
        class: "btn small", type: "button", "data-control": name, "aria-label": `${label} ${row.label || "this button"}`, title: label,
        disabled: index + step < 0 || index + step >= rows.length,
        onclick: () => {
          [rows[index], rows[index + step]] = [rows[index + step], rows[index]];
          const target = index + step;
          // At the top or bottom the arrow just pressed is disabled: keep focus on the other one.
          const name2 = (target === 0 && step < 0) || (target === rows.length - 1 && step > 0) ? (step < 0 ? "down" : "up") : name;
          draw([target, name2]);
        },
      }, symbol);
      return h("div", { class: `modbtn-row${row.visible ? "" : " is-hidden"}` },
        h("div", { class: "modbtn-move" }, move(-1, "up", "↑", "Move up"), move(1, "down", "↓", "Move down")),
        h("label", { class: "check" },
          h("input", { type: "checkbox", checked: row.visible, onchange: (e) => { row.visible = e.target.checked; draw([index, "shown"]); }, "data-control": "shown" }),
          "Shown"),
        h("input", { class: "modbtn-label", value: row.label, maxlength: 16, placeholder: "Label", "aria-label": "Button label", oninput: (e) => { row.label = e.target.value; drawPreview(); } }),
        h("input", { class: "modbtn-codes mono", value: row.codes, placeholder: "DICOM codes, e.g. CR, DX", "aria-label": "DICOM modality codes", oninput: (e) => { row.codes = e.target.value; drawPreview(); } }),
        h("input", { class: "modbtn-title", value: row.title, maxlength: 80, placeholder: "Description (shown on hover)", "aria-label": "Description", oninput: (e) => { row.title = e.target.value; drawPreview(); } }),
        h("button", { class: "btn small danger", type: "button", onclick: () => { rows.splice(index, 1); draw(); } }, "Remove"));
    }));
    drawPreview();
    if (focus) {
      const control = list.children[focus[0]] && list.children[focus[0]].querySelector(`[data-control="${focus[1]}"]`);
      if (control) control.focus();
    }
  }
  draw();

  const node = h("div", { class: "wide modbtn-editor" }, preview, list,
    h("div", { class: "filter-actions" },
      h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ label: "", codes: "", title: "", visible: true }); draw([rows.length - 1, "shown"]); } }, "Add button"),
      h("button", { class: "btn small", type: "button", onclick: () => { rows = clone(defaults); draw(); } }, "Restore default buttons")));

  return {
    node,
    get: () => rows.map((r) => ({ label: r.label.trim(), codes: parseCodes(r.codes), title: r.title.trim(), visible: r.visible })),
  };
}

/**
 * The tabs inside Settings. The first four show parts of ONE form (the settings saved
 * together with the "Save settings" button: `sections` are the ids given to `section()`
 * in buildSettingsView). The others are separate screens whose changes apply at once.
 */
const SETTINGS_TABS = [
  { id: "general", label: "General", sections: ["branding", "search", "viewers"] },
  { id: "appearance", label: "Appearance", sections: ["theme", "simplified"] },
  { id: "orthanc", label: "Orthanc", sections: ["orthanc"] },
  { id: "network", label: "Network & sharing", sections: ["network", "sharing", "email"] },
  { id: "security", label: "Security & roles", sections: ["security", "roles"] },
  { id: "users", label: "Users", build: buildUsersView },
  { id: "nodes", label: "DICOM nodes", build: buildNodesView },
  { id: "audit", label: "Audit log", build: buildAuditView },
];

/** The "Settings" tab: the complete configuration of the portal, organised in tabs. */
function buildSettingsView() {
  let doc = null; // { settings, listeners, defaults, permissions } from the server
  const noticeSlot = h("div", { class: "notice", role: "status", hidden: true });
  const tabBar = h("div", { class: "subtabs", role: "tablist", "aria-label": "Settings" });
  const formHost = h("div", { class: "view" }, h("div", { class: "panel loading" }, "Loading settings..."));
  const screenHost = h("div", { class: "view" });
  const view = h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Settings")), noticeSlot, tabBar, formHost, screenHost);

  if (!SETTINGS_TABS.some((t) => t.id === state.settingsTab)) state.settingsTab = SETTINGS_TABS[0].id;

  /**
   * Shows the current tab. Form tabs only hide the other sections, so values typed in one
   * tab are kept (and saved) when switching to another. The other screens are rebuilt each
   * time they are opened, so they always show fresh data.
   */
  const currentTab = () => SETTINGS_TABS.find((t) => t.id === state.settingsTab);

  /** Shows only the form sections of the current tab (the whole form is hidden on the other screens). */
  function showSections() {
    const tab = currentTab();
    formHost.hidden = !tab.sections;
    formHost.querySelectorAll("[data-section]").forEach((s) => { s.hidden = !tab.sections || !tab.sections.includes(s.dataset.section); });
  }

  function showTab() {
    const tab = currentTab();
    for (const button of tabBar.querySelectorAll("[data-tab]")) {
      const selected = button.dataset.tab === tab.id;
      button.setAttribute("aria-selected", String(selected));
      button.tabIndex = selected ? 0 : -1;
    }
    showSections();
    screenHost.replaceChildren(...(tab.build ? [tab.build()] : []));
    screenHost.hidden = !tab.build;
  }

  /** Tab bar keyboard support: Left / Right / Home / End move between tabs. */
  tabBar.addEventListener("keydown", (event) => {
    const keys = { ArrowLeft: -1, ArrowRight: 1, Home: "first", End: "last" };
    if (!(event.key in keys)) return;
    event.preventDefault();
    const index = SETTINGS_TABS.findIndex((t) => t.id === state.settingsTab);
    const step = keys[event.key];
    const next = step === "first" ? 0 : step === "last" ? SETTINGS_TABS.length - 1 : (index + step + SETTINGS_TABS.length) % SETTINGS_TABS.length;
    state.settingsTab = SETTINGS_TABS[next].id;
    showTab();
    tabBar.querySelector(`[data-tab="${state.settingsTab}"]`).focus();
  });

  SETTINGS_TABS.forEach((t, i) => {
    // A thin divider between the settings form and the separate screens.
    if (i > 0 && t.build && !SETTINGS_TABS[i - 1].build) tabBar.append(h("span", { class: "subtabs-divider", "aria-hidden": "true" }));
    tabBar.append(h("button", {
      type: "button", role: "tab", "data-tab": t.id,
      onclick: () => { state.settingsTab = t.id; showTab(); },
    }, t.label));
  });

  /** What the last save did (and a reconnect link if this page's port changed). */
  function showNotices(notices) {
    noticeSlot.replaceChildren(...notices);
    noticeSlot.hidden = notices.length === 0;
  }

  async function load() {
    try {
      doc = await api("GET", "/api/admin/settings");
      draw([]);
    } catch (e) {
      formHost.replaceChildren(h("div", { class: "panel empty" }, e.message));
      toastError(e);
    }
  }

  /** Draws the form from `doc`. `messages` = what the server did on the last save. */
  function draw(messages) {
    const s = doc.settings;
    const inputs = {};

    /** One labelled control for a field of SETTINGS_FIELDS. */
    const field = (def) => {
      const value = s[def.key];
      let input;
      if (def.type === "textarea") {
        input = h("textarea", { rows: 3, maxlength: 500 }, value);
      } else if (def.type === "select") {
        input = h("select", null, def.options.map(([v, text]) => h("option", { value: v, selected: v === value }, text)));
      } else if (def.type === "checkbox") {
        input = h("input", { type: "checkbox", checked: !!value });
      } else {
        input = h("input", { type: def.type || "text", value: value ?? "", min: def.min, max: def.max, maxlength: def.maxlength, placeholder: def.placeholder });
      }
      inputs[def.key] = input;
      const control = def.type === "checkbox" ? h("label", { class: "check" }, input, def.label) : h("label", null, def.label, input);
      return h("div", { class: def.wide ? "setting wide" : "setting" }, control, def.hint ? h("p", { class: "hint" }, def.hint) : null);
    };

    /** A titled card of the form. `id` says which tab shows it (SETTINGS_TABS). */
    const section = (id, title, intro, ...children) => h("section", { class: "panel settings-section", "data-section": id },
      h("h2", null, title), intro ? h("p", { class: "muted" }, intro) : null, h("div", { class: "settings-grid" }, children));

    // ---- Orthanc connection. Orthanc can be protected by an account (user name + password) OR
    // have no users at all and only accept a list of allowed clients / IP addresses. The
    // selector makes that choice explicit; "without an account" means the portal sends no
    // credentials, which the server stores as an empty user name (and no password).
    const [urlField, userField] = SETTINGS_FIELDS.orthanc.map(field);
    const modeSelect = h("select", { id: "orthanc-mode" },
      h("option", { value: "account", selected: Boolean(s.orthanc_username) }, "With an account (user name and password)"),
      h("option", { value: "anonymous", selected: !s.orthanc_username }, "Without an account (Orthanc only accepts a list of allowed clients / IP addresses)"));
    const modeField = h("div", { class: "setting wide" },
      h("label", null, "How does Orthanc let the portal in?", modeSelect),
      h("p", { class: "hint" }, "Choose \"without an account\" when Orthanc has no users and only accepts certain computers. Then make sure Orthanc's allowed list includes the address the portal connects from. In Docker that is the server's address, or an address of Docker's private network (172.x.x.x), not 127.0.0.1."));

    // The password is write-only: the browser never receives it.
    const password = h("input", { type: "password", autocomplete: "new-password", placeholder: s.orthanc_password_set ? "A password is saved. Leave empty to keep it." : "No password saved" });
    const clearPassword = h("input", { type: "checkbox" });
    const passwordField = h("div", { class: "setting" },
      h("label", null, "Orthanc password", password),
      h("label", { class: "check" }, clearPassword, "Remove the saved password"));
    const withoutAccount = () => modeSelect.value === "anonymous";
    /** The user name to use or test: always empty when the portal connects without an account. */
    const accountName = () => (withoutAccount() ? "" : inputs.orthanc_username.value);
    const showAccountFields = () => { userField.hidden = passwordField.hidden = withoutAccount(); };
    modeSelect.addEventListener("change", showAccountFields);
    showAccountFields();
    const testBox = h("div", { class: "test-result", hidden: true, role: "status" });
    const testButton = h("button", { class: "btn", type: "button" }, "Test connection");
    testButton.addEventListener("click", async () => {
      testButton.disabled = true;
      testBox.hidden = false;
      testBox.className = "test-result";
      testBox.textContent = "Testing...";
      try {
        const r = await api("POST", "/api/admin/orthanc/test", {
          // Without an account the user name is sent empty; the server then ignores any saved password.
          url: inputs.orthanc_url.value, username: accountName(), password: withoutAccount() ? "" : password.value,
        });
        testBox.className = `test-result ${r.ok ? "ok" : "bad"}`;
        const details = r.version
          ? ` Plugins: ${r.plugins.join(", ") || "none"}. DICOMweb: ${r.dicomweb ? "yes" : "no"}. OHIF: ${r.ohif ? "yes" : "no"}. DICOM nodes: ${r.modalities}.`
          : "";
        testBox.textContent = r.message + (r.ok ? "." : "") + details;
      } catch (e) {
        testBox.className = "test-result bad";
        testBox.textContent = e.message;
      } finally {
        testButton.disabled = false;
      }
    });

    const rolesEditor = buildRolesEditor(s.roles, doc.defaults.roles, doc.permissions);

    // ---- Email: the mail server password is write-only (like Orthanc's), and a test button.
    const smtpPassword = h("input", { type: "password", autocomplete: "new-password", placeholder: s.smtp_password_set ? "A password is saved. Leave empty to keep it." : "No password saved" });
    const clearSmtpPassword = h("input", { type: "checkbox" });
    const smtpPasswordField = h("div", { class: "setting" },
      h("label", null, "Password on the mail server", smtpPassword),
      h("label", { class: "check" }, clearSmtpPassword, "Remove the saved password"));
    const testTo = h("input", { type: "text", placeholder: "you@example.com", autocomplete: "off", "aria-label": "Send a test email to" });
    const testMailBox = h("div", { class: "test-result", hidden: true, role: "status" });
    const testMailButton = h("button", { class: "btn", type: "button" }, "Send a test email");
    testMailButton.addEventListener("click", async () => {
      testMailButton.disabled = true;
      testMailBox.hidden = false;
      testMailBox.className = "test-result";
      testMailBox.textContent = "Sending...";
      try {
        const r = await api("POST", "/api/admin/email/test", { to: testTo.value });
        testMailBox.className = `test-result ${r.ok ? "ok" : "bad"}`;
        testMailBox.textContent = r.message;
      } catch (e) {
        testMailBox.className = "test-result bad";
        testMailBox.textContent = e.message;
      } finally {
        testMailButton.disabled = false;
      }
    });
    const emailFields = SETTINGS_FIELDS.email.map(field);
    emailFields.splice(4, 0, smtpPasswordField);
    const testMailField = h("div", { class: "setting wide" },
      h("span", { class: "field-label" }, "Send a test email to (uses the saved settings: save first)"),
      h("div", { class: "inline-field" }, testTo, testMailButton), testMailBox);

    // ---- Appearance: light / dark and the colours. Every change is previewed at once on this
    // page; it is kept only when saved (leaving the page without saving puts the saved colours back).
    const themeMode = h("select", null,
      [["auto", "Automatic (follows each computer's light or dark setting)"], ["light", "Always light"], ["dark", "Always dark"]]
        .map(([v, text]) => h("option", { value: v, selected: v === s.theme_mode }, text)));
    inputs.theme_mode = themeMode;
    const colorFields = COLOR_FIELDS.map((def) => {
      const saved = s[def.key] || "";
      const picker = h("input", { type: "color", value: saved || def.fallback, disabled: !saved, "aria-label": `${def.label} colour` });
      const useDefault = h("input", { type: "checkbox", checked: !saved });
      useDefault.addEventListener("change", () => { picker.disabled = useDefault.checked; previewTheme(); });
      picker.addEventListener("input", previewTheme);
      // Read like the other inputs when the form is saved: "" = the built-in colour.
      inputs[def.key] = { type: "color", get value() { return useDefault.checked ? "" : picker.value; } };
      return h("div", { class: "setting" },
        h("span", { class: "field-label" }, def.label),
        h("div", { class: "color-setting" }, picker, h("label", { class: "check" }, useDefault, "Default")),
        h("p", { class: "hint" }, def.hint));
    });
    function previewTheme() {
      applyTheme({
        mode: themeMode.value, accent: inputs.color_accent.value, header: inputs.color_header.value,
        sidebar: inputs.color_sidebar.value, background: inputs.color_background.value,
      });
    }
    themeMode.addEventListener("change", previewTheme);
    const themeFields = [
      h("div", { class: "setting wide" }, h("label", null, "Light or dark", themeMode),
        h("p", { class: "hint" }, "The colours below replace the built-in ones in both themes, on the login page and on the invitation page too.")),
      ...colorFields,
    ];
    const modalityEditor = buildModalityButtonsEditor(s.modality_buttons, doc.defaults.modality_buttons);

    // Docker (LISTENERS_FROM_ENV): the addresses must match the published ports, so they are
    // set in the .env file of docker-compose and only shown here.
    const networkFields = SETTINGS_FIELDS.network.map(field);
    if (doc.listeners_from_env) {
      for (const key of ["bind_addr", "remote_bind_addr"]) inputs[key].disabled = true;
    }
    const networkIntro = doc.listeners_from_env
      ? "Two listeners: the intranet one (the whole portal, for staff) and the internet one (ONLY invitation links, for the Cloudflare Tunnel). Their addresses are fixed by the server's environment (Docker): to change a port, edit INTRANET_PORT / INTERNET_PORT in the .env file next to docker-compose.yml and run \"docker compose up -d\"."
      : "Two listeners: the intranet one (the whole portal, for staff) and the internet one (ONLY invitation links, for the Cloudflare Tunnel). Changing an address re-opens the listener right away; if the new address cannot be opened (for example because another program uses that port), nothing is changed.";

    const listenerInfo = h("ul", { class: "listener-status wide" }, doc.listeners.map((l) =>
      h("li", null, h("b", null, l.name), ": ", l.running ? `listening on ${l.addr}` : "not running")));

    // ---- Notices: what the last save did, and a reconnect link if this page's port is no longer the intranet port
    const notices = [];
    messages.forEach((m) => notices.push(h("p", null, m)));
    const intranet = doc.listeners[0];
    const newPort = intranet && intranet.addr ? intranet.addr.split(":").pop() : "";
    const thisPort = window.location.port || (window.location.protocol === "https:" ? "443" : "80");
    if (newPort && newPort !== thisPort) {
      const url = `${window.location.protocol}//${window.location.hostname}:${newPort}/`;
      notices.push(h("p", null, "The intranet interface now uses port ", newPort, ". This page was opened on another port; continue at ", h("a", { href: url }, url), "."));
    }

    const save = h("button", { class: "btn primary", type: "submit" }, "Save settings");
    const form = h("form", {
      class: "settings-form",
      onsubmit: async (event) => {
        event.preventDefault();
        if (!withoutAccount() && !inputs.orthanc_username.value.trim()) {
          toast("Enter the Orthanc user name, or choose \"Without an account\".", "error");
          return;
        }
        save.disabled = true;
        const shownInterface = currentInterface();
        // Collect the values of every control into the object the server expects.
        const body = {};
        for (const [key, input] of Object.entries(inputs)) {
          body[key] = input.type === "checkbox" ? input.checked : input.type === "number" ? Number(input.value) : input.value;
        }
        body.roles = rolesEditor.get();
        body.modality_buttons = modalityEditor.get();
        if (smtpPassword.value) body.smtp_password = smtpPassword.value;
        if (clearSmtpPassword.checked) body.clear_smtp_password = true;
        if (withoutAccount()) {
          // No account: no user name and no stored password (the server enforces this too).
          body.orthanc_username = "";
          body.clear_orthanc_password = true;
        } else {
          if (password.value) body.orthanc_password = password.value;
          if (clearPassword.checked) body.clear_orthanc_password = true;
        }
        try {
          doc = await api("PUT", "/api/admin/settings", body);
          await refreshBrand();
          // Viewer buttons, thumbnails and the "Share..." dialog follow the features: take the
          // server's fresh view of them (it also knows whether Orthanc has the OHIF plugin), so
          // what was just saved applies right away, without signing in again.
          try { state.user.features = (await api("GET", "/api/me")).features; } catch { /* keep the old ones */ }
          refreshOrthancStatus(true);
          toast("Settings saved.");
          // The administrator's own view may have changed (simplified view switched on or off,
          // or the view of their role): redraw the whole page, staying on Settings.
          if (currentInterface() !== shownInterface) { renderApp(); return; }
          draw(doc.messages || []);
        } catch (e) {
          toastError(e);
          save.disabled = false;
        }
      },
    },
      section("branding", "Branding", "Make the portal look like your practice. The window title, the portal name and the login notice are saved with the button below; the logo and the website icon are saved as soon as you choose a file.",
        SETTINGS_FIELDS.portal.map(field),
        buildBrandImagePicker("logo", "Logo", "PNG, JPEG, WebP, GIF or SVG, up to 1 MB. Shown in the top bar (about 32 px high, on a light plate so any logo is readable) and on the login page.", "image/png,image/jpeg,image/webp,image/gif,image/svg+xml"),
        buildBrandImagePicker("favicon", "Website icon", "PNG, ICO, SVG, JPEG, WebP or GIF, up to 256 KB. Square, at least 64 x 64 pixels. Shown in the browser tab and in bookmarks.", "image/png,image/x-icon,image/vnd.microsoft.icon,image/svg+xml,image/jpeg,image/webp,image/gif")),
      section("search", "Modality buttons", "The quick filters above the Studies and Series lists. Put them in the order your staff use them and hide the ones nobody needs. Each button searches for one or more DICOM modality codes (RX = CR + DX). The \"Other codes\" box is always shown, so any code can still be searched.",
        modalityEditor.node),
      section("viewers", "Viewers and uploads", null, SETTINGS_FIELDS.viewers.map(field)),
      section("theme", "Colours", "Match the portal to your practice's colours. Changes are previewed on this page at once and kept when you save. The logo, the portal name and the window title are in the General tab.",
        themeFields),
      section("simplified", "Simplified view", "A reduced layout for staff who mostly look at recent studies and upload new ones: the logo, the practice name and the total of studies at the top, a column of icons (Home, Search, Shares, Uploads, Transfers, Help, Configuration, Log out) on the left, and a home page with the most recent studies and an area to drop files into. Choose which roles use it in Security & roles > Roles.",
        SETTINGS_FIELDS.simplified.map(field)),
      section("orthanc", "Orthanc connection", "The PACS this portal works with. Saved changes apply immediately, without restarting.",
        urlField, modeField, userField, passwordField,
        h("div", { class: "setting wide" }, testButton, testBox)),
      section("network", "Network", networkIntro, networkFields, listenerInfo),
      section("sharing", "Share links (invitations for people outside)", "A staff member with the share permission can invite somebody outside: a link plus a generated password that opens ONE study, to look at online and to download a limited number of times, until the link expires. These are the defaults offered when creating a link, and the limits nobody can exceed.", SETTINGS_FIELDS.sharing.map(field)),
      section("email", "Email (sending share invitations)", "The Share windows can send the invitation by email (optional, for each link) when a mail server is set here. The emails are plain text; the password is included only when the staff member ticks it.",
        emailFields, testMailField),
      section("security", "Sessions and sign-in security", null, SETTINGS_FIELDS.security.map(field)),
      section("roles", "Roles", "Shortcuts that pre-tick permissions when you create or edit a user (Users tab), and the view (normal or simplified) of the users who have that role, used while the simplified view is switched on (Appearance tab). Users keep their own permissions: changing a role's permissions does not change existing users, but changing its view applies to them at their next page load.", rolesEditor.node),
      h("div", { class: "settings-actions" },
        save,
        h("span", { class: "muted" }, "Saves the General, Appearance, Orthanc, Network & sharing and Security & roles tabs together.")));

    formHost.replaceChildren(form);
    showNotices(notices);
    showSections();
  }

  showTab();
  load();
  return view;
}

/** The "DICOM nodes" tab: remote systems Orthanc can send studies to. */
function buildNodesView() {
  const container = h("section", { class: "panel" });

  async function load() {
    container.replaceChildren(h("div", { class: "loading" }, "Loading..."));
    try {
      const nodes = await api("GET", "/api/admin/nodes");
      container.replaceChildren(nodes.length ? renderTable(nodes) : h("div", { class: "empty" }, "No DICOM node yet. Add the remote PACS or workstation you want to send studies to."));
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  function renderTable(nodes) {
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Name", "Description", "AE title", "Address", "Send", "Ping", "Actions"].map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
      h("tbody", null, nodes.map((n) => h("tr", null,
        h("td", null, h("span", { class: "patient" }, n.id)),
        h("td", null, n.description || h("span", { class: "muted" }, "-")),
        h("td", { class: "mono" }, n.aet),
        h("td", { class: "mono" }, `${n.host}:${n.port}`),
        h("td", null, h("span", { class: `chip ${n.allow_store ? "status-on" : "status-off"}` }, n.allow_store ? "allowed" : "blocked")),
        h("td", null, h("span", { class: `chip ${n.allow_echo ? "status-on" : "status-off"}` }, n.allow_echo ? "allowed" : "blocked")),
        h("td", null, h("div", { class: "actions" },
          h("button", {
            class: "btn small", type: "button",
            onclick: async (event) => {
              const button = event.currentTarget;
              button.disabled = true;
              try { await api("POST", `/api/admin/nodes/${encodeURIComponent(n.id)}/echo`); toast(`${n.id} answered the DICOM ping.`); } catch (e) { toastError(e); }
              button.disabled = false;
            },
          }, "Test"),
          h("button", { class: "btn small", type: "button", onclick: () => openNodeDialog(n, load) }, "Edit"),
          h("button", {
            class: "btn small danger", type: "button",
            onclick: async () => {
              if (!confirm(`Delete the DICOM node "${n.id}"?`)) return;
              try { await api("DELETE", `/api/admin/nodes/${encodeURIComponent(n.id)}`); toast("Node deleted."); load(); } catch (e) { toastError(e); }
            },
          }, "Delete"))))))));
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "DICOM nodes"),
      h("button", { class: "btn primary", type: "button", onclick: () => openNodeDialog(null, load) }, "Add node")),
    h("p", { class: "muted" }, "Remote DICOM systems (a PACS, a workstation...) that users with the send permission can send studies to. They are stored in Orthanc itself; they survive an Orthanc restart only if Orthanc runs with DicomModalitiesInDatabase enabled."),
    container);
}

/** Dialog to add (`node` = null) or edit a DICOM node. */
function openNodeDialog(node, reload) {
  const isNew = !node;
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const name = h("input", { required: true, value: node ? node.id : "", disabled: !isNew, placeholder: "radiology-ws", autocomplete: "off" });
  const aet = h("input", { required: true, maxlength: 16, value: node ? node.aet : "", placeholder: "RADWS01", autocomplete: "off" });
  const host = h("input", { required: true, value: node ? node.host : "", placeholder: "192.168.1.50", autocomplete: "off" });
  const port = h("input", { required: true, type: "number", min: 1, max: 65535, value: node ? node.port : 104 });
  const description = h("input", { maxlength: 80, value: node ? node.description || "" : "", placeholder: "e.g. Radiology information system", autocomplete: "off" });
  const allowStore = h("input", { type: "checkbox", checked: node ? node.allow_store : true });
  const allowEcho = h("input", { type: "checkbox", checked: node ? node.allow_echo : true });

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      try {
        await api("PUT", `/api/admin/nodes/${encodeURIComponent(isNew ? name.value.trim() : node.id)}`, {
          aet: aet.value, host: host.value, port: Number(port.value), description: description.value,
          allow_store: allowStore.checked, allow_echo: allowEcho.checked,
        });
        dialog.close();
        toast("DICOM node saved.");
        reload();
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
      }
    },
  },
    h("h2", null, isNew ? "Add a DICOM node" : `Edit ${node.id}`),
    error,
    h("label", null, "Name (used in Orthanc)", name),
    h("label", null, "AE title of the remote system", aet),
    h("label", null, "Host name or IP address", host),
    h("label", null, "DICOM port", port),
    h("label", null, "Description (what the staff see, e.g. on the Transfers page)", description),
    h("label", { class: "check" }, allowStore, "Orthanc may send images to it (C-STORE)"),
    h("label", { class: "check" }, allowEcho, "Orthanc may ping it (C-ECHO)"),
    h("p", { class: "muted" }, "The remote system must also be configured to accept associations from Orthanc's AE title and address."),
    h("div", { class: "dialog-actions" },
      h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"),
      h("button", { class: "btn primary", type: "submit" }, "Save node"))));

  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/* ====================================================================
   10. START-UP
   ==================================================================== */

/** Asks the server whether a session already exists (cookie), then draws the page. */
(async function boot() {
  try { state.brand = await api("GET", "/api/public"); } catch { /* keep the default identity */ }
  applyBrand();
  try { state.user = await api("GET", "/api/me"); } catch { state.user = null; }
  renderApp();
})();
