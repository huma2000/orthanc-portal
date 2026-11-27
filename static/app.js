/*
  app.js - the whole browser side of the Orthanc Portal (vanilla JavaScript).

  The server only exposes a JSON API (see src/routes/mod.rs); this file draws
  the interface and calls it. No framework and no build step: edit, save,
  rebuild the Rust program (the file is embedded in the executable) and reload.

  Sections
    1. Settings and state
   1b. Translations                 (t(), the dictionary of the chosen language)
    2. Tiny DOM helper `h()`        (builds elements safely, never uses innerHTML; translates its texts)
    3. API helper `api()`           (adds the CSRF header, handles errors / expired sessions)
    4. Formatting helpers           (DICOM names, dates)
    5. Shell: login, sign out, colours (theme)
   5b. The portal's layout           (header, icon column, home page with recent studies)
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
 * which ones are shown and in what order (Settings > Search & labels > Modality buttons); the
 * server sends the visible ones, in order, with the user's features.
 */
const modalityButtons = () => (state.user && state.user.features && state.user.features.modality_buttons) || [];

/** Global UI state. `user` is null while signed out. */
const state = {
  user: null,     // { id, username, permissions: ["read", ...], features: { ohif, thumbnails, share: {...} } }
  simpleView: "home", // id of the visible page (see SIMPLE_PAGES)
  simpleSearch: "studies", // which search the Search page shows: "studies" or "series"
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
   1b. TRANSLATIONS
   The interface is written in English in this file. For another language the server sends a
   dictionary, static/i18n/<code>.json: { "texts": { "English text": "translation" }, "messages": {...} }
   ("messages" = the server's messages, see translateMessage). Plain texts given to h()
   (children, and the placeholder / title / aria-label / alt / data-tip attributes) are translated
   when the element is built, so most code just writes English. A text with values in it goes
   through t("Send {n} studies", { n }): the dictionary key keeps the {names}. Messages from the
   server are translated by translateMessage(): a key's {names} then match any value.
   The portal's language is a setting (Settings > Appearance); each person may choose another one
   for their browser (the menu in the header and on the login page), remembered in this browser.
   ==================================================================== */

const LANGUAGE_KEY = "portal.language";
const i18n = {
  lang: "en",
  dict: null,      // the texts of the interface; null = English: nothing to translate
  messages: {},    // the server's messages
  patterns: [],    // the server's messages with {names}: [{ regex, names, template }]
  locale: undefined, // dates and numbers: the browser's own format in English, Spanish (Spain) in Spanish
};

/** The translation of a text as it is (surrounding spaces kept), or the text itself. */
function lookup(text) {
  if (!i18n.dict || typeof text !== "string" || !text) return text;
  const direct = i18n.dict[text];
  if (direct !== undefined) return direct;
  const parts = /^(\s*)([\s\S]*?)(\s*)$/.exec(text);
  if (parts[2] === text || !parts[2]) return text;
  const core = i18n.dict[parts[2]];
  return core === undefined ? text : parts[1] + core + parts[3];
}

/** Translates `text` and fills its {names} from `values`: t("Send {n} studies", { n: 3 }). */
function t(text, values) {
  const out = lookup(text);
  return values ? out.replace(/\{(\w+)\}/g, (all, name) => (name in values ? String(values[name]) : all)) : out;
}

/** "1 series" / "3 series" and "1 image" / "3 images": the singular has its own key (it differs in Spanish). */
const seriesCount = (n) => (Number(n) === 1 ? t("1 series") : t("{series} series", { series: n ?? "?" }));
const imagesCount = (n) => (Number(n) === 1 ? t("1 image") : t("{n} images", { n }));

/**
 * A message of the server in the chosen language: an exact key, or a key with {names} whose
 * literal parts match ("Unknown setting: {name}"). The values are kept (translated themselves when
 * they are a known text). Unknown messages stay in English.
 */
function translateMessage(message) {
  if (!i18n.dict || typeof message !== "string" || !message) return message;
  const text = message.trim();
  if (i18n.messages[text] !== undefined) return i18n.messages[text];
  const exact = lookup(text);
  if (exact !== text) return exact;
  for (const p of i18n.patterns) {
    const m = p.regex.exec(text);
    if (m) return p.template.replace(/\{(\w+)\}/g, (all, name) => { const i = p.names.indexOf(name); return i < 0 ? all : translateValue(m[i + 1]); });
  }
  return message;
}

/** A value inside a server message: translated when it is itself a known text or message. */
const translateValue = (value) => (i18n.messages[value] !== undefined ? i18n.messages[value] : lookup(value));

/** Turns the keys with {names} into patterns (longest first, so the most precise one wins). */
function buildPatterns(dict) {
  const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return Object.keys(dict).filter((key) => /\{\w+\}/.test(key)).sort((a, b) => b.length - a.length).map((key) => {
    const names = [];
    const source = key.split(/(\{\w+\})/).map((piece) => {
      const name = /^\{(\w+)\}$/.exec(piece);
      if (!name) return escape(piece);
      names.push(name[1]);
      return "([\\s\\S]+?)";
    }).join("");
    return { regex: new RegExp(`^${source}$`), names, template: dict[key] };
  });
}

/** The languages offered (from /api/public): [[code, name]]. */
const languages = () => (state.brand && state.brand.languages) || [["en", "English"]];

/** The language of this browser: the person's own choice, or the portal's. */
function chosenLanguage() {
  let mine = "";
  try { mine = localStorage.getItem(LANGUAGE_KEY) || ""; } catch { /* private window */ }
  const offered = languages().map(([code]) => code);
  if (offered.includes(mine)) return mine;
  return offered.includes(state.brand && state.brand.language) ? state.brand.language : "en";
}

/** Loads the dictionary of a language (English needs none). Falls back to English on any problem. */
async function loadLanguage(lang) {
  if (lang === i18n.lang && (lang === "en" || i18n.dict)) return;
  let data = null;
  if (lang !== "en") {
    try {
      const response = await fetch(`/static/i18n/${encodeURIComponent(lang)}.json`, { credentials: "same-origin" });
      if (response.ok) data = await response.json();
    } catch { /* stays in English */ }
  }
  const dict = data && data.texts ? data.texts : null;
  i18n.lang = dict ? lang : "en";
  i18n.dict = dict;
  i18n.messages = (dict && data.messages) || {};
  i18n.patterns = dict ? buildPatterns(i18n.messages) : [];
  i18n.locale = dict ? ({ es: "es-ES" }[lang] || lang) : undefined;
  document.documentElement.lang = i18n.lang;
}

/** A person chose a language in the menu: remembered in this browser, and the page is redrawn. */
async function setLanguage(lang) {
  try { localStorage.setItem(LANGUAGE_KEY, lang); } catch { /* only for this page */ }
  await loadLanguage(lang);
  renderApp();
}

/** The language menu (header and login page); none when the portal offers only one language. */
function languageMenu() {
  const list = languages();
  if (list.length < 2) return null;
  return h("select", {
    class: "language-select", "aria-label": "Language", title: "Language",
    onchange: (event) => setLanguage(event.target.value),
  }, list.map(([code, name]) => h("option", { value: code, selected: code === i18n.lang, lang: code }, name)));
}

/* ====================================================================
   2. DOM HELPER
   ==================================================================== */

/**
 * h("div", { class: "x", onclick: fn }, child1, [child2, child3], "text")
 * Creates an element. Strings become text nodes (safe), arrays are flattened,
 * null/false children are skipped. Props named onXxx add event listeners.
 * Texts and the attributes people read are translated (section 1b).
 */
const TRANSLATED_ATTRIBUTES = new Set(["placeholder", "title", "aria-label", "alt", "data-tip"]);
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (["value", "checked", "disabled", "hidden", "selected"].includes(key)) el[key] = value;
    else el.setAttribute(key, value === true ? "" : TRANSLATED_ATTRIBUTES.has(key) ? lookup(value) : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(typeof child === "string" ? lookup(child) : String(child)));
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
    throw new ApiError(t("Cannot reach the portal. Check your network connection."), 0);
  }
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* the answer was not JSON */ }

  if (!response.ok) {
    // 401 while signed in = the session expired or the account was disabled.
    if (response.status === 401 && state.user && url !== "/api/login") sessionExpired();
    throw new ApiError(translateMessage((data && data.error) || text.slice(0, 200)) || t("Request failed ({status})", { status: response.status }), response.status);
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

/** How dates and names are written (Settings > Appearance); the login page uses the public copy. */
const formatOptions = () => (state.user && state.user.features && state.user.features.format) || (state.brand && state.brand.format) || {};

/** DICOM person name "DOE^JOHN^^^" -> "DOE, JOHN" (or "JOHN DOE", or as stored: Settings > Appearance). */
function formatName(raw) {
  if (!raw) return t("(no name)");
  const style = formatOptions().name;
  if (style === "dicom") return raw.replace(/\^+$/, "");
  const parts = raw.split("^").map((p) => p.trim()).filter(Boolean);
  if (parts.length < 2) return parts[0] || raw;
  return style === "first_last" ? `${parts.slice(1).join(" ")} ${parts[0]}` : `${parts[0]}, ${parts.slice(1).join(" ")}`;
}

const MONTHS = {
  en: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
  es: ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sept", "oct", "nov", "dic"],
};

/** DICOM date "20240517" -> "2024-05-17" (or 17/05/2024...: Settings > Appearance). Anything else is shown unchanged. */
function formatDicomDate(value) {
  if (!/^\d{8}$/.test(value || "")) return value || "";
  const [y, m, d] = [value.slice(0, 4), value.slice(4, 6), value.slice(6, 8)];
  switch (formatOptions().date) {
    case "dmy": return `${d}/${m}/${y}`;
    case "mdy": return `${m}/${d}/${y}`;
    case "dmy_dots": return `${d}.${m}.${y}`;
    case "long": return `${Number(d)} ${(MONTHS[i18n.lang] || MONTHS.en)[Number(m) - 1] || m} ${y}`;
    default: return `${y}-${m}-${d}`;
  }
}

/** Unix seconds -> local date/time string. */
const formatTimestamp = (unixSeconds) => (unixSeconds ? new Date(unixSeconds * 1000).toLocaleString(i18n.locale) : t("never"));

/** Coloured modality badge (colours defined in app.css through data-m). */
const chip = (modality) => tint(h("span", { class: "chip", "data-m": modality, title: modalityName(modality) }, modality || "?"), modalityColor(modality));

/**
 * Gives a tag or a filter button the colour chosen in Settings (a modality button, a label). The
 * stylesheet blends it with the page's own colours, so one choice reads well in light and dark;
 * a pressed button shows the colour itself, with black or white text (whichever reads better).
 * Through the DOM (allowed by the Content-Security-Policy, unlike style attributes in HTML).
 */
function tint(el, color) {
  if (!isColor(color)) return el;
  el.classList.add("tinted");
  el.style.setProperty("--chip", color);
  el.style.setProperty("--chip-ink", inkOn(color));
  return el;
}
/**
 * The Orthanc servers this person may use (switched on, in order). Every server control of the page
 * appears only with two or more: with one server the portal looks exactly as before.
 */
const orthancServers = () => (state.user && state.user.orthanc_servers) || [];
const severalServers = () => orthancServers().length > 1;
const serverInfo = (key) => orthancServers().find((x) => x.key === key);
const serverName = (key) => (serverInfo(key) || { label: key }).label;
/** A server's badge, in its colour (Settings). */
const serverBadge = (key) => tint(h("span", { class: "chip server-chip", title: t("Stored on {server}", { server: serverName(key) }) }, serverName(key)),
  (serverInfo(key) || {}).color);
/** The badges of a study or series: every server holding it (search results), or its own. */
const serverBadges = (item) => (item.servers && item.servers.length ? item.servers : [item.server || "main"]).map((key) => {
  const badge = serverBadge(key);
  // A study on several servers: the copy its buttons use is marked.
  if (item.servers && item.servers.length > 1 && key === item.server) badge.classList.add("server-current");
  return badge;
});
/** `&server=key` for the viewer addresses: their requests then read the study from that server. */
const serverParam = (key) => (severalServers() && key ? `&server=${encodeURIComponent(key)}` : "");

/** The colour of a modality code (Settings > Search & labels > Modality buttons), or undefined. */
const modalityColor = (code) => ((state.brand && state.brand.modality_colors) || {})[code];
/** The colour of a label (Settings > Search & labels > Label colours), or undefined. */
const labelColor = (label) => (searchOptions().label_colors || {})[label];

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
  if (!code) return t("Modality not recorded");
  const button = modalityButtons().find((b) => b.codes.length === 1 && b.codes[0] === code && b.title);
  const name = button ? button.title : MODALITY_NAMES[code] && t(MODALITY_NAMES[code]);
  return name ? `${code}: ${name}` : t("{code}: modality code", { code });
}

/** Grey placeholder for empty values. */
const dash = () => h("span", { class: "muted" }, "-");

/* ====================================================================
   5. SHELL: LOGIN, NAVIGATION, SIGN OUT
   ==================================================================== */

/** Opens Settings (Configuration) on one of its tabs (e.g. "orthanc" from the Orthanc notice). */
function openSettings(tab) {
  state.simpleView = "settings";
  state.settingsTab = tab;
  renderApp();
}

/** Opens the list of studies (Search > Studies). */
function openStudies() {
  if (!can("read")) return;
  state.simpleView = "search";
  state.simpleSearch = "studies";
  renderApp();
}

/** Draws the whole page: the login form when signed out, otherwise the portal (section 5b). */
function renderApp() {
  root.replaceChildren();
  // Colours as saved (a preview in Settings > Appearance ends when the user leaves the page).
  applyTheme(state.brand.theme);
  if (!state.user) {
    root.append(buildLogin());
    return;
  }
  renderSimplified();
}

/**
 * What identifies the practice in the header and on the login page: the uploaded
 * logo (or the default mark) and, unless an administrator hid it, the portal name.
 * `asHeading` makes the name an <h1> (login page) instead of a <span> (header).
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
    slot.replaceChildren(...brandContent(Number(slot.dataset.size), slot.dataset.heading === "1").filter(Boolean)); // no name = null, not the text "null"
  });
}

/** Reloads the identity from the server (after an administrator changes it). */
async function refreshBrand() {
  state.brand = await api("GET", "/api/public");
  const before = i18n.lang;
  await loadLanguage(chosenLanguage());
  applyBrand();
  return before !== i18n.lang; // the language changed (Settings > Appearance): the caller redraws
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
        const result = await api("POST", "/api/login", { username: username.value, password: password.value });
        // Two-step sign-in: the password was right, the code of the authenticator app comes next.
        if (result && result.two_factor) {
          const step = buildTwoFactorStep(result, finishLogin);
          form.replaceWith(step);
          step.querySelector("input[name=code]").focus();
          return;
        }
        finishLogin(result);
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
      form,
      languageMenu() ? h("div", { class: "login-language" }, languageMenu()) : null));
}

/** Signed in: draw the portal (and show the recovery codes of a two-step sign-in set up just now). */
function finishLogin(me, recoveryCodes) {
  state.user = me;
  state.notice = "";
  renderApp();
  if (recoveryCodes && recoveryCodes.length) showRecoveryCodes(recoveryCodes);
}

/** An SVG text (a QR code drawn by the server) as a picture. */
function svgImage(svgText, alt) {
  const img = h("img", { class: "qr", alt, width: 220, height: 220 });
  img.src = URL.createObjectURL(new Blob([svgText], { type: "image/svg+xml" }));
  img.addEventListener("load", () => URL.revokeObjectURL(img.src), { once: true });
  return img;
}

/** The QR code and the secret of a two-step sign-in being set up (`material` = { qr, secret }). */
function twoFactorSetupBlock(material) {
  return h("div", { class: "totp-setup" },
    material.qr ? svgImage(material.qr, t("QR code to scan with an authenticator app")) : null,
    h("ol", null,
      h("li", null, "Install an authenticator app on your phone if you do not have one (Microsoft Authenticator, Google Authenticator, FreeOTP...)."),
      h("li", null, "In the app, add an account and scan this QR code. Cannot scan? Type this key instead:"),
      h("li", { class: "plain" }, h("span", { class: "secret-box" }, h("span", { class: "password mono" }, material.secret), copyButton("Copy key", () => material.secret.replaceAll(" ", "")))),
      h("li", null, "Type the 6-digit code the app shows to confirm.")));
}

/**
 * The second step of the sign-in: the code of the app, or the set-up when the portal requires it and
 * this account has none yet. `done(me, recoveryCodes)` is called once signed in.
 */
function buildTwoFactorStep(result, done) {
  const setup = result.two_factor === "setup";
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const code = h("input", { name: "code", autocomplete: "one-time-code", inputmode: "numeric", required: true, maxlength: 20, spellcheck: "false", placeholder: "123456" });
  const submit = h("button", { class: "btn primary", type: "submit" }, setup ? "Confirm and sign in" : "Sign in");
  return h("form", {
    class: "two-factor-step",
    onsubmit: async (event) => {
      event.preventDefault();
      submit.disabled = true;
      error.hidden = true;
      try {
        const me = await api("POST", "/api/login/two-factor", { ticket: result.ticket, code: code.value });
        const codes = me.recovery_codes;
        delete me.recovery_codes;
        done(me, codes);
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
        code.select();
      }
    },
  },
    h("h2", null, "Two-step sign-in"),
    setup
      ? [h("p", null, "This portal asks for a code from your phone at every sign-in, on top of the password. Set it up now (it takes a minute):"), twoFactorSetupBlock(result)]
      : h("p", { class: "muted" }, "Enter the 6-digit code shown by the authenticator app on your phone. Lost your phone? Type one of your recovery codes instead."),
    error,
    h("label", null, "Code", code),
    submit,
    h("button", { class: "btn small", type: "button", onclick: () => { state.notice = ""; renderApp(); } }, "Start again"));
}

/** The recovery codes, shown once (after setting up two-step sign-in, or asking for new ones). */
function showRecoveryCodes(codes) {
  const text = codes.join("\n");
  const dialog = h("dialog", null, h("div", { class: "dialog-form" },
    h("h2", null, "Your recovery codes"),
    h("p", null, "If you lose your phone, each of these codes signs you in once instead of the code of the app. Keep them somewhere safe (printed, or in a password manager): they are shown only now."),
    h("ul", { class: "recovery-codes" }, codes.map((c) => h("li", { class: "mono" }, c))),
    h("div", { class: "dialog-actions" },
      copyButton("Copy the codes", () => text),
      h("button", { class: "btn primary", type: "button", onclick: () => dialog.close() }, "I have saved them"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
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
   5b. THE PORTAL'S LAYOUT
   Every staff member sees the same layout (only the icons their permissions allow):

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
   chose so).
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
  transfers: ["M3 12h10.5", "M10 8.5l3.5 3.5-3.5 3.5", "M16 4.5h5v15h-5z", "M18.5 8h.01", "M18.5 11h.01"], // an arrow into another system
  help: [{ c: [12, 12, 9] }, "M9.6 9.4a2.5 2.5 0 1 1 3.4 2.4c-.6.3-1 .8-1 1.5v.6", { c: [12, 16.9, 0.35] }],
  // Buttons of a study, of a link...
  weasis: ["M3 4.5h18v11.5H3z", "M8.5 20.5h7", "M12 16v4.5"],                       // a desktop screen
  ohif: ["M3 5h18v14H3z", "M3 9h18", { c: [5.5, 7, 0.4] }, { c: [7.5, 7, 0.4] }, "M8 14.5s1.6-2.5 4-2.5 4 2.5 4 2.5-1.6 2.5-4 2.5-4-2.5-4-2.5z"], // a browser window with an eye
  send: ["M21 3 10.5 13.5", "M21 3l-6.5 18-4-7.5L3 9.5z"],                          // a paper plane
  download: ["M12 3.5v11.5", "M7.5 10.5 12 15l4.5-4.5", "M4 17v3.5h16V17"],
  refresh: ["M19.5 12a7.5 7.5 0 1 1-2.2-5.3", "M19.5 4v4.5H15"],
  link: ["M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1", "M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"],
  patient: [{ c: [12, 8, 3.5] }, "M5 20.5c0-3.9 3.1-6.5 7-6.5s7 2.6 7 6.5"],            // a person: their other studies
  tag: ["M3.5 12.5V4h8.5l8.5 8.5-8.5 8.5z", { c: [8, 8.5, 1.2] }],                    // a label
  revoke: [{ c: [12, 12, 8.5] }, "M6 6l12 12"],
  key: [{ c: [7.5, 15.5, 4] }, "M10.5 12.5 20 3", "M16.5 6.5l2.5 2.5", "M14 9l2 2"],
  edit: ["M4 20h4L19 9l-4-4L4 16z", "M13.5 6.5l4 4"],
  more: [{ c: [5.5, 12, 1.3] }, { c: [12, 12, 1.3] }, { c: [18.5, 12, 1.3] }],          // three dots: more actions
  viewers: ["M3 5h18v14H3z", "M8 19v-4h8v4", "M7 9h10", "M7 12h6"],                     // another viewer
  images: ["M3.5 5h13v13h-13z", "M7.5 2.5h13v13", "M3.5 15l4-4 3 3 2-2 4 4"],            // a stack of images
  pdf: ["M6 2.5h8l4.5 4.5v14.5H6z", "M14 2.5V7h4.5", "M9 13h6", "M9 16.5h6"],            // a document
  retrieve: ["M12 3v11", "M7.5 9.5 12 14l4.5-4.5", "M3 14.5h4v5h10v-5h4"],               // into this archive
  worklist: ["M8 4.5h11", "M8 10h11", "M8 15.5h11", { c: [4.5, 4.5, 1] }, { c: [4.5, 10, 1] }, { c: [4.5, 15.5, 1] }, "M3 20.5h18"],
  account: [{ c: [12, 12, 9] }, { c: [12, 10, 3] }, "M6.6 18.3c1.3-2.1 3.1-3.1 5.4-3.1s4.1 1 5.4 3.1"], // my account
  qr: ["M4 4h6v6H4z", "M14 4h6v6h-6z", "M4 14h6v6H4z", "M14 14h2v2h-2z", "M18 18h2v2h-2z", "M14 18h2", "M18 14h2"],
  disc: [{ c: [12, 12, 9] }, { c: [12, 12, 2.5] }, "M12 3a9 9 0 0 1 9 9"],                    // patient CD / USB
  servers: ["M4 4.5h16v6H4z", "M4 13.5h16v6H4z", "M7 7.5h.01", "M7 16.5h.01", "M10.5 7.5h6", "M10.5 16.5h6"], // Orthanc servers (the copies of a study)
  inbox: ["M3.5 13.5 6 5h12l2.5 8.5V19h-17z", "M3.5 13.5h5l1 2.5h5l1-2.5h5", "M12 6.5v5", "M9.8 9.4 12 11.6l2.2-2.2"], // files coming in
};

/**
 * Turns the properties of a text button into those of an icon button: the label becomes the
 * tooltip (see the tooltip code below) and the accessible name.
 */
const asIcon = (props, label) => ({ ...props, class: `${props.class || ""} icon-btn`.trim(), "data-tip": label, "aria-label": label, title: null });

/**
 * A button that shows only an icon (the buttons of a study, of a link...). `tip` is the description
 * shown on hover (and the accessible name); `text` is its short name, kept as `data-name`.
 * `tag` is "button", "a" or "summary".
 */
function actionButton(tag, iconName, text, tip, props) {
  return h(tag, { ...asIcon(props, tip), "data-name": text }, icon(iconName));
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
 * The pages of the portal, top to bottom. `needs` = permission required to see the
 * icon (null = everyone). `bottom` pages sit at the foot of the column, above "Log out".
 */
const SIMPLE_PAGES = [
  { id: "home", label: "Home", icon: "home", needs: null, build: buildHomeView },
  { id: "search", label: "Search", icon: "search", needs: "read", build: buildSimpleSearch },
  { id: "shares", label: "Shares", icon: "share", needs: "share", build: buildSharesView },
  { id: "upload", label: "Uploads", icon: "upload", needs: "upload", build: buildUploadView },
  { id: "transfers", label: "Transfers", icon: "transfers", needs: null, when: () => can("send") || !!opts().retrieve, build: buildTransfersView },
  { id: "retrieve", label: "Retrieve", icon: "retrieve", needs: null, when: () => !!opts().retrieve, build: buildRetrieveView },
  { id: "worklists", label: "Worklists", icon: "worklist", needs: null, when: () => !!opts().worklists, build: buildWorklistsView },
  { id: "account", label: "My account", icon: "account", needs: null, bottom: true, build: buildAccountView,
    when: () => opts().account_self_service !== false || (state.user.features.two_factor || "off") !== "off" },
  { id: "help", label: "Help", icon: "help", needs: null, bottom: true, build: buildHelpView },
  { id: "settings", label: "Configuration", icon: "settings", needs: "admin", bottom: true, build: buildSettingsView },
];

/**
 * The Search page: the Studies and the Series searches, behind two small tabs.
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

/** Options of the home page and the icons, sent by the server (Settings > Appearance). */
const simpleOptions = () => (state.user && state.user.features && state.user.features.simplified) || {};

/** Draws the portal: header, icon column and the current page. */
function renderSimplified() {
  const options = simpleOptions();
  const pages = SIMPLE_PAGES.filter((p) => (!p.needs || can(p.needs)) && (!p.when || p.when()));
  if (!pages.some((p) => p.id === state.simpleView)) state.simpleView = "home";
  const current = pages.find((p) => p.id === state.simpleView);
  // The address carries the filters of the Search page (see showSearchInAddress); elsewhere it is plain.
  if (current.id !== "search" && location.search) { try { history.replaceState(null, "", "/"); } catch { /* fine */ } }

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
      pages.filter((p) => p.bottom).map(pageButton),
      sideButton(t("Log out ({user})", { user: state.user.username }), "logout", signOut)));

  const showTotal = options.show_total !== false && can("read");
  const head = h("header", { class: "simple-head" },
    h("div", { class: "brand brand-slot", "data-size": "40" }, ...brandContent(40, false)),
    h("div", { class: "head-right" },
      showTotal ? h("div", { class: "simple-total" }, "Total studies: ", h("b", { id: "total-studies" }, totalStudies.text)) : null,
      languageMenu()));

  // Red notice (administrators only) when Orthanc is unreachable or too old.
  const banner = h("div", { id: "orthanc-banner", class: "banner", role: "alert", hidden: true });
  const main = h("main", { class: "simple-main" }, banner, current.build());

  root.append(h("div", { class: options.show_labels ? "simple labels" : "simple" }, head, side, main));
  paintOrthancBanner();
  refreshOrthancStatus();
  if (showTotal) refreshTotalStudies();
}

/**
 * Fills in "Total studies: N" in the header. The header is redrawn at every
 * click on an icon, so the number is asked from Orthanc at most once a minute, unless `force`
 * (after an upload).
 */
const totalStudies = { text: "...", at: 0 };
async function refreshTotalStudies(force = false) {
  const show = () => {
    const slot = document.getElementById("total-studies");
    if (slot) { slot.textContent = totalStudies.text; slot.title = totalStudies.title || ""; }
  };
  show();
  if (!document.getElementById("total-studies") || (!force && Date.now() - totalStudies.at < 60000)) return;
  totalStudies.at = Date.now();
  try {
    const stats = await api("GET", "/api/stats");
    totalStudies.text = typeof stats.studies === "number" ? stats.studies.toLocaleString(i18n.locale) : "?";
    // With several servers, the count of each one in the tooltip.
    totalStudies.title = (stats.servers || []).length > 1
      ? stats.servers.map((x) => `${x.label}: ${typeof x.studies === "number" ? x.studies.toLocaleString(i18n.locale) : "?"}`).join(" · ")
      : "";
  } catch {
    totalStudies.text = "?"; // Orthanc unreachable: the administrators' red notice explains it
    totalStudies.at = 0;     // try again at the next page
  }
  show();
}

/** Reloads the recent studies of the home page, if it is on screen (set by buildHomeView). */
let reloadRecentStudies = null;

/** The home page: the most recent studies, and a drop zone to upload. */
/**
 * A switch "Auto-refresh · 24 s" that calls `refresh` every few seconds (Settings > Appearance) while
 * `node` is on the page, counting down the seconds left before the next update. Each user can pause
 * it (remembered in this browser). The countdown waits while the tab is hidden, a window is open or
 * series are unfolded, so nothing changes under the user's mouse. `.restart()` starts it again from
 * the full interval (after the Refresh button).
 */
function autoRefresh(key, node, refresh) {
  const seconds = Math.max(5, Number(simpleOptions().refresh_seconds) || 30);
  const storageKey = `portal.autorefresh.${key}`;
  let on = true;
  try { on = localStorage.getItem(storageKey) !== "off"; } catch { /* private window: on */ }
  let left = seconds;
  // The words change every second; the name read by screen readers stays the same.
  const countdown = h("span", { class: "auto-refresh-time", "aria-hidden": "true" });
  const button = h("button", {
    type: "button", class: "chip-toggle auto-refresh has-tip", "data-name": "auto-refresh",
    "aria-label": t("Refresh automatically every {n} seconds", { n: seconds }),
  }, "Auto-refresh · ", countdown);

  /** "24 s", "refreshing", "waiting" or "paused", and the tooltip that explains it. */
  function show(waiting = false) {
    button.setAttribute("aria-pressed", String(on));
    countdown.textContent = !on ? t("paused") : waiting ? t("waiting") : left <= 0 ? t("refreshing") : `${left} s`;
    button.dataset.tip = !on ? t("Paused. Click to refresh the list automatically again.")
      : waiting ? t("Waiting: the list is not refreshed while a window is open, series are shown or the tab is in the background.")
        : t("The list refreshes itself every {n} seconds (next in {left} s). Click to pause.", { n: seconds, left: Math.max(left, 0) });
  }
  button.addEventListener("click", () => {
    on = !on;
    left = seconds;
    try { localStorage.setItem(storageKey, on ? "on" : "off"); } catch { /* not remembered */ }
    show();
  });
  button.restart = () => { left = seconds; show(); };
  show();

  const timer = setInterval(() => {
    if (!button.isConnected || !node.isConnected) { clearInterval(timer); return; }
    if (!on) return;
    // Series unfolded anywhere on the page (the list itself, or "Recently viewed" above it) also wait.
    const waiting = document.hidden || !!document.querySelector("dialog[open]") || !!node.querySelector("tr.series-row:not([hidden])")
      || !!document.querySelector("main tr.series-row:not([hidden])");
    if (waiting) { show(true); return; }
    if (left <= 0) left = seconds; // "refreshing" was shown for one second
    left -= 1;
    show();
    if (left <= 0) refresh();
  }, 1000);
  return button;
}

function buildHomeView() {
  const options = simpleOptions();
  const parts = [];

  // What this person opened last (their own list; Settings > Appearance sets its length).
  if (can("read") && state.user.features.recently_viewed > 0) {
    const body = h("div", { class: "recent-body" });
    const section = h("section", { class: "recent recently-viewed", "aria-labelledby": "viewed-title", hidden: true },
      h("div", { class: "recent-head" },
        h("h2", { id: "viewed-title" }, "Recently viewed"),
        h("div", { class: "head-actions" },
          h("button", { class: "btn small", type: "button", onclick: async () => {
            try { await api("DELETE", "/api/recent"); section.hidden = true; } catch (e) { toastError(e); }
          } }, "Clear the list"))),
      body);
    api("GET", "/api/recent").then((list) => {
      if (!list.length) return;
      body.replaceChildren(renderStudiesTable(list));
      section.hidden = false;
    }).catch(() => { /* the list is a convenience: nothing to say when it cannot be read */ });
    parts.push(section);
  }

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

    const recentAuto = options.recent_auto_refresh ? autoRefresh("recent", body, () => { load(); refreshTotalStudies(true); }) : null;
    const section = h("section", { class: "recent", "aria-labelledby": "recent-title" },
      h("div", { class: "recent-head" },
        h("h2", { id: "recent-title" }, options.recent_title || "Recent studies"),
        h("div", { class: "head-actions" },
          recentAuto,
          actionButton("button", "refresh", "Refresh", "Refresh the recent studies", {
            class: "btn small", type: "button", onclick: () => { load(); if (recentAuto) recentAuto.restart(); },
          }))),
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

/**
 * "I opened this study": goes to the top of my Recently viewed list (home page). Fire and forget;
 * the same study is noted at most once a minute.
 */
const notedViews = new Map();
function noteViewed(study) {
  if (!study || !study.id || !study.study_instance_uid || !(state.user && state.user.features.recently_viewed > 0)) return;
  const now = Date.now();
  if (now - (notedViews.get(study.id) || 0) < 60000) return;
  notedViews.set(study.id, now);
  api("POST", `/api/recent/${study.id}`, { uid: study.study_instance_uid }).catch(() => {});
}

/** Details of one study (opened from a card): who, when, what, the actions and the series. */
function openStudyDialog(study) {
  noteViewed(study);
  const seriesSlot = h("div", null, h("div", { class: "loading" }, "Loading series..."));
  const detail = (label, value) => (value ? [h("dt", null, label), h("dd", null, value)] : null);
  const labelsSlot = h("dd", null, labelChips(study.labels));
  const values = {
    patient_id: study.patient_id, birth_date: formatDicomDate(study.patient_birth_date), sex: study.patient_sex,
    other_ids: study.other_patient_ids, study_date: formatDicomDate(study.study_date), description: study.study_description,
    accession: study.accession_number, referring: study.referring_physician ? formatName(study.referring_physician) : "",
    requesting: study.requesting_physician ? formatName(study.requesting_physician) : "", institution: study.institution,
    modalities: study.modalities.length ? h("span", null, study.modalities.map(chip)) : null,
    series: study.instances_count != null ? `${seriesCount(study.series_count)}, ${imagesCount(study.instances_count)}` : seriesCount(study.series_count),
    study_uid: study.study_instance_uid,
    server: severalServers() ? h("span", null, serverBadges(study)) : null,
  };
  const fields = [...(opts().detail_fields || ["patient_id", "sex", "birth_date", "study_date", "description", "modalities", "series", "labels"]), "server"]
    .filter((k) => k !== "labels" || labelsOn());
  const dialog = h("dialog", { class: "study-dialog" }, h("div", { class: "dialog-form" },
    h("h2", null, formatName(study.patient_name)),
    h("dl", { class: "share-details" },
      fields.map((k) => (k === "labels" ? [h("dt", null, "Labels"), labelsSlot] : detail(STUDY_DETAIL_TITLES[k], values[k])))),
    h("div", { class: "actions" }, studyActions(study, () => labelsSlot.replaceChildren(...[labelChips(study.labels)].flat()),
      (copy) => { dialog.close(); openStudyDialog(copyOf(study, copy)); })),
    seriesSlot,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();

  api("GET", `/api/studies/${study.id}/series`)
    .then((list) => seriesSlot.replaceChildren(renderSeriesBox(list, study)))
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
 * `size` asks the server for a bigger picture (the cards of the home page).
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

/** Remembers what the user typed in each search form (and how the list is sorted) while they browse other pages. */
const searchMemory = { studies: {}, series: {} };

/** Settings > Search & labels, as sent by the server: columns, search boxes, live search, endless lists, labels. */
const searchOptions = () => (state.user && state.user.features && state.user.features.search) || {};
const canEditLabels = () => opts().labels !== false && !!searchOptions().can_edit_labels;

/**
 * Every search box the Studies search can offer; Settings > Search & labels chooses which ones are
 * shown, and in which order. A box can have two controls (from / to). `name` is the parameter sent
 * to /api/studies; a value between double quotes ("P-001") must match exactly.
 */
const STUDY_FIELDS = {
  patient_name: { title: "Patient name", controls: [{ name: "patient_name", label: "Patient name", placeholder: "DOE or DOE*" }] },
  patient_id: { title: "Patient ID", controls: [{ name: "patient_id", label: "Patient ID" }] },
  birth_date: { title: "Birth date (from / to)", controls: [{ name: "birth_from", label: "Birth date from", type: "date" }, { name: "birth_to", label: "Birth date to", type: "date" }] },
  sex: { title: "Sex", controls: [{ name: "patient_sex", label: "Sex", type: "select", options: [["", "Any"], ["F", "Female"], ["M", "Male"], ["O", "Other"]] }] },
  other_ids: { title: "Other patient IDs", controls: [{ name: "other_patient_ids", label: "Other patient IDs" }] },
  description: { title: "Study description", controls: [{ name: "description", label: "Study description" }] },
  accession: { title: "Accession number", controls: [{ name: "accession_number", label: "Accession number" }] },
  referring: { title: "Referring physician", controls: [{ name: "referring_physician", label: "Referring physician" }] },
  requesting: { title: "Requesting physician", controls: [{ name: "requesting_physician", label: "Requesting physician" }] },
  institution: { title: "Institution", controls: [{ name: "institution", label: "Institution" }] },
  modality: { title: "Modality buttons", controls: [{ name: "modality", label: "Modality", type: "modalities" }] },
  dates: { title: "Study date (from / to)", controls: [{ name: "date_from", label: "Study date from", type: "date" }, { name: "date_to", label: "Study date to", type: "date" }] },
  labels: { title: "Labels", controls: [{ name: "labels", label: "Labels", type: "labels" }] },
};
/** Filters that have no box of their own (they arrive through a link from another program). */
const EXTRA_STUDY_CONTROLS = [{ name: "study_uid", label: "StudyInstanceUID" }];
const DEFAULT_STUDY_FIELDS = ["patient_name", "patient_id", "description", "modality", "dates", "labels"];

/** Configuration of the Studies search. `fields` become the filter form. */
const STUDY_SEARCH = {
  id: "studies",
  title: "Studies",
  endpoint: "/api/studies",
  get fields() {
    return (searchOptions().fields || DEFAULT_STUDY_FIELDS).filter((key) => key !== "labels" || labelsOn())
      .flatMap((key) => (STUDY_FIELDS[key] ? STUDY_FIELDS[key].controls : []));
  },
  get allControls() {
    return [...Object.entries(STUDY_FIELDS).filter(([key]) => key !== "labels" || labelsOn()).flatMap(([, f]) => f.controls), ...EXTRA_STUDY_CONTROLS];
  },
  defaultSort: { sort: "study_date", dir: "desc" },
  renderTable: renderStudiesTable,
  rows: (study) => studyRows(study),
};

/** Configuration of the Series search. */
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
  get allControls() { return this.fields; },
  defaultSort: { sort: "", dir: "" },
  renderTable: renderSeriesTable,
  rows: (series) => seriesRow(series),
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
    button: tint(h("button", {
      type: "button", class: "chip-toggle", title: chip.title, "aria-pressed": "false",
      onclick: () => {
        const active = chip.codes.every((c) => selected.has(c));
        chip.codes.forEach((c) => (active ? selected.delete(c) : selected.add(c)));
        refresh();
        onChange();
      },
    }, chip.label), chip.color),
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

/* ---- Orthanc servers ---- */

/** The servers this browser last chose to search (remembered per person, per browser). */
const SERVERS_KEY = "portal-search-servers";
function rememberedServers() {
  try {
    const saved = JSON.parse(localStorage.getItem(`${SERVERS_KEY}-${state.user.id}`) || "null");
    if (Array.isArray(saved)) return saved.filter((key) => serverInfo(key));
  } catch { /* private window, or storage blocked */ }
  return null;
}
function rememberServers(keys) {
  try { localStorage.setItem(`${SERVERS_KEY}-${state.user.id}`, JSON.stringify(keys)); } catch { /* not kept */ }
}

/**
 * "Servers: (All) (Main) (Archive)": which Orthanc servers a search asks. Starts from the address
 * (a copied link), else this browser's last choice, else the servers "searched by default".
 * `.value` = the chosen keys separated by commas ("" = the default ones).
 */
function buildServerPicker(initial, onChange) {
  const defaults = () => orthancServers().filter((x) => x.searched_by_default).map((x) => x.key);
  const chosen = new Set();
  const load = (list) => {
    chosen.clear();
    const keys = list ? list.split(",").map((k) => k.trim()).filter((k) => serverInfo(k)) : (rememberedServers() || defaults());
    (keys.length ? keys : defaults()).forEach((k) => chosen.add(k));
    refresh();
  };
  const all = h("button", {
    type: "button", class: "chip-toggle", "aria-pressed": "false", title: "Search every server",
    onclick: () => { orthancServers().forEach((x) => chosen.add(x.key)); changed(); },
  }, "All");
  const buttons = orthancServers().map((server) => ({
    key: server.key,
    button: tint(h("button", {
      type: "button", class: "chip-toggle", "aria-pressed": "false", title: t("Search {server}", { server: server.label }),
      onclick: () => {
        if (chosen.has(server.key)) { if (chosen.size > 1) chosen.delete(server.key); } else chosen.add(server.key);
        changed();
      },
    }, server.label), server.color),
  }));
  function refresh() {
    all.setAttribute("aria-pressed", String(chosen.size === orthancServers().length));
    for (const { key, button } of buttons) button.setAttribute("aria-pressed", String(chosen.has(key)));
  }
  function changed() { refresh(); rememberServers([...chosen]); onChange(); }
  load(initial);
  return {
    node: h("div", { class: "mod-picker server-picker" }, all, buttons.map((b) => b.button)),
    get value() { return orthancServers().filter((x) => chosen.has(x.key)).map((x) => x.key).join(","); },
    set value(list) { load(list); }, // "Clear filters" gives "": this browser's choice again
  };
}

/** "Archive did not answer (...): results from the other servers only." above a list. */
function unavailableNotice(page, retry) {
  if (!page.unavailable || !page.unavailable.length) return null;
  return h("p", { class: "login-notice server-notice", role: "status" },
    page.unavailable.map((u) => t("{server} did not answer ({error}).", { server: u.label, error: translateMessage(u.error) })).join(" "),
    " ", lookup("The results come from the other servers only."), " ",
    h("button", { class: "btn small", type: "button", onclick: retry }, "Try again"));
}

/* ---- Labels ---- */

/** The labels with their study counts, asked once and kept until a label changes. */
let labelsCache = null;
function loadLabels() {
  if (!labelsCache) {
    labelsCache = api("GET", "/api/labels").then((r) => r.labels).catch((e) => { labelsCache = null; toastError(e); return []; });
  }
  return labelsCache;
}

/** Labels as small tags. */
const labelChips = (labels) => (labels && labels.length ? labels.map((l) => tint(h("span", { class: "chip label-chip", "data-label": l }, l), labelColor(l))) : dash());

/**
 * Labels search box: a toggle per label (with its number of studies), "Without labels", and
 * "any / all" once several are chosen. Its `.value` is kept in the search memory as text:
 * "" | "none" | "any:a,b" | "all:a,b"; `addParams` turns it into the parameters of /api/studies.
 */
function buildLabelPicker(initial, onChange) {
  let selected = new Set(), mode = "any", none = false, labels = [];
  const box = h("div", { class: "label-picker" }, h("span", { class: "muted" }, "Loading labels..."));

  function load(text) {
    selected = new Set(); mode = "any"; none = false;
    if (text === "none") none = true;
    else if (text) {
      const [m, list] = text.includes(":") ? text.split(":", 2) : ["any", text];
      mode = m === "all" ? "all" : "any";
      list.split(",").map((l) => l.trim()).filter(Boolean).forEach((l) => selected.add(l));
    }
  }
  function draw() {
    const toggle = (pressed, text, title, onclick, color) => tint(h("button", { type: "button", class: "chip-toggle label-toggle", "aria-pressed": String(pressed), title, onclick }, text), color);
    const known = labels.map((l) => l.name);
    const all = [...labels, ...[...selected].filter((n) => !known.includes(n)).map((name) => ({ name, count: null }))];
    box.replaceChildren(...[
      ...all.map((l) => toggle(selected.has(l.name), l.count == null ? l.name : `${l.name} (${l.count})`, t("Studies labelled {label}", { label: l.name }),
        () => { none = false; if (selected.has(l.name)) selected.delete(l.name); else selected.add(l.name); draw(); onChange(); }, labelColor(l.name))),
      toggle(none, "Without labels", "Studies that have no label at all", () => { none = !none; selected.clear(); draw(); onChange(); }),
      selected.size > 1
        ? h("select", { class: "label-mode", "aria-label": "Match", onchange: (e) => { mode = e.target.value; onChange(); } },
          h("option", { value: "any", selected: mode === "any" }, "any of them"),
          h("option", { value: "all", selected: mode === "all" }, "all of them"))
        : null,
      all.length ? null : h("span", { class: "muted" }, "No label yet."),
    ].filter(Boolean)); // replaceChildren() would write a null as the text "null"
  }
  load(initial || "");
  loadLabels().then((list) => { labels = list; draw(); });
  return {
    node: box,
    get value() { return none ? "none" : selected.size ? `${mode}:${[...selected].join(",")}` : ""; },
    set value(text) { load(text || ""); draw(); },
    addParams(params) {
      if (none) params.set("no_labels", "true");
      else if (selected.size) { params.set("labels", [...selected].join(",")); if (mode === "all") params.set("labels_mode", "all"); }
    },
  };
}

/**
 * The labels of a study: shown, taken off (×) and added (with suggestions; only the allowed
 * ones when an administrator restricted them). `onChange` redraws the row.
 */
function openLabelsDialog(study, onChange) {
  const allowed = searchOptions().allowed_labels || [];
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const list = h("div", { class: "label-list" });
  const datalist = h("datalist", { id: "label-suggestions" });
  const input = h("input", { list: "label-suggestions", maxlength: 64, autocomplete: "off", "aria-label": "Label to add",
    placeholder: allowed.length ? "Choose one of the labels" : "A word, e.g. urgent or second-opinion" });
  loadLabels().then((labels) => datalist.replaceChildren(...[...new Set([...allowed, ...labels.map((l) => l.name)])]
    .filter((n) => !study.labels.includes(n)).map((n) => h("option", { value: n }))));

  function draw() {
    list.replaceChildren(...(study.labels.length
      ? study.labels.map((l) => tint(h("span", { class: "chip label-chip", "data-label": l }, l,
        h("button", { type: "button", class: "chip-remove", "aria-label": t("Take the label {label} off", { label: l }), title: "Take it off", onclick: () => change("DELETE", l) }, "×")), labelColor(l)))
      : [h("span", { class: "muted" }, "No label yet.")]));
  }
  async function change(method, label) {
    error.hidden = true;
    try {
      const r = await api(method, `/api/studies/${study.id}/labels/${encodeURIComponent(label)}`);
      study.labels = r.labels;
      labelsCache = null; // the counts changed
      draw();
      onChange();
    } catch (e) {
      error.textContent = e.message;
      error.hidden = false;
    }
  }
  const dialog = h("dialog", null, h("form", {
    onsubmit: (event) => { event.preventDefault(); const label = input.value.trim(); if (label) { change("PUT", label); input.value = ""; } },
  },
    h("h2", null, "Labels"),
    h("p", null, h("b", null, formatName(study.patient_name)), " ", formatDicomDate(study.study_date), study.study_description ? `, ${study.study_description}` : ""),
    list, error,
    h("div", { class: "inline-field" }, input, h("button", { class: "btn primary", type: "submit" }, "Add")),
    datalist,
    h("p", { class: "hint" }, allowed.length
      ? t("Allowed labels: {labels}.", { labels: allowed.join(", ") })
      : "Letters, digits, - and _ only (no spaces). Labels are shared: everyone sees them and can search by them."),
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  draw();
  dialog.showModal();
  input.focus();
}

/* ---- Links to a search ---- */

/** Opens the Search page with these filters (and nothing else), e.g. all the studies of a patient. */
function openSearchWith(kind, filters) {
  document.querySelectorAll("dialog[open]").forEach((d) => d.close());
  searchMemory[kind] = { ...filters };
  state.simpleView = "search";
  state.simpleSearch = kind;
  renderApp();
}

/** All the studies of the patient of `study` (exact Patient ID, or exact name without an ID). */
function openPatientStudies(study) {
  openSearchWith("studies", study.patient_id ? { patient_id: `"${study.patient_id}"` } : { patient_name: `"${study.patient_name}"` });
}

/** The address of the page follows the search, so it can be bookmarked or sent ("Copy link"). */
function showSearchInAddress(kind, params) {
  const shown = new URLSearchParams(params);
  shown.delete("limit");
  shown.delete("offset");
  if (kind === "series") shown.set("search", "series");
  const query = shown.toString();
  try { history.replaceState(null, "", query ? `/?${query}` : "/"); } catch { /* not possible here: the page still works */ }
}

/**
 * A link with filters (e.g. /?patient_id=P-001&date_from=2026-01-01&sort=patient_name, or
 * /?study_uid=1.2.3 from another program) opens the Search page with them. Read once, at start-up
 * (before signing in, if needed).
 */
function readSearchFromAddress() {
  const query = new URLSearchParams(location.search);
  if (![...query.keys()].length) return;
  const kind = query.get("search") === "series" ? "series" : "studies";
  const filters = {};
  for (const [key, value] of query) if (!["search", "labels_mode", "no_labels"].includes(key)) filters[key] = value;
  if (query.get("no_labels") === "true") filters.labels = "none";
  else if (filters.labels) filters.labels = `${query.get("labels_mode") === "all" ? "all" : "any"}:${filters.labels}`;
  searchMemory[kind] = filters;
  state.simpleView = "search";
  state.simpleSearch = kind;
}

/**
 * Generic search page: filter form + results + paging (Previous / Next, or the next results
 * loaded at the end of the list when "endless lists" is on).
 * `cfg` (STUDY_SEARCH / SERIES_SEARCH) says which fields to show, which API endpoint to call
 * and how to draw the rows.
 */
function buildSearchView(cfg) {
  const memory = searchMemory[cfg.id];
  const options = searchOptions();
  const inputs = {};
  let offset = 0;
  let hasMore = false;
  let loading = false;
  let latestSearch = 0; // number of the newest search; older answers are ignored (see run())
  let shownCount = 0;

  // The boxes chosen in Settings, plus any filter that came in another way (a link, "all the
  // studies of this patient") without a box of its own: shown too, so no filter is ever hidden.
  const chosen = cfg.fields;
  const extra = cfg.allControls.filter((c) => memory[c.name] && !chosen.some((f) => f.name === c.name));
  const fields = [...chosen, ...extra];

  // Searching while typing (Settings > Search & labels): after a pause, and once enough letters are typed.
  let typingTimer = null;
  const typed = (input) => {
    if (!options.live) return;
    clearTimeout(typingTimer);
    typingTimer = setTimeout(() => {
      const v = input.value.trim();
      if (!v || v.replaceAll('"', "").length >= (options.min_chars || 3)) run(0);
    }, 350);
  };

  // With several Orthanc servers: which ones to search, first.
  const serverNodes = [];
  if (severalServers()) {
    const picker = buildServerPicker(memory.servers || "", () => run(0));
    inputs.servers = picker;
    serverNodes.push(h("div", { class: "field wide" }, h("span", { class: "field-label" }, "Servers"), picker.node));
  }

  // One labelled control per field, pre-filled from memory. Modality and label buttons search
  // again as soon as they are toggled.
  const fieldNodes = fields.map((field) => {
    if (field.type === "modalities" || field.type === "labels") {
      const picker = field.type === "modalities"
        ? buildModalityPicker(memory[field.name] || "", () => run(0))
        : buildLabelPicker(memory[field.name] || "", () => run(0));
      inputs[field.name] = picker; // exposes .value like a normal input
      return h("div", { class: "field wide" }, h("span", { class: "field-label" }, field.label), picker.node);
    }
    let input;
    if (field.type === "select") {
      input = h("select", { name: field.name, onchange: () => { if (options.live) run(0); } },
        field.options.map(([v, text]) => h("option", { value: v, selected: v === (memory[field.name] || "") }, text)));
    } else {
      input = h("input", { name: field.name, type: field.type || "text", value: memory[field.name] || "", placeholder: field.placeholder || "" });
      if (field.type === "date") input.addEventListener("change", () => { if (options.live) run(0); });
      else input.addEventListener("input", () => typed(input));
    }
    inputs[field.name] = input;
    return h("label", null, field.label, input);
  });

  const results = h("section", { class: "panel" });

  /** The current sort, and a click on a column title: that column, or the other direction. */
  const sorting = () => ({
    sort: memory.sort || cfg.defaultSort.sort,
    dir: memory.dir || cfg.defaultSort.dir,
    onSort: (key) => {
      const current = memory.sort || cfg.defaultSort.sort;
      const dir = memory.dir || cfg.defaultSort.dir;
      memory.dir = key === current ? (dir === "asc" ? "desc" : "asc") : (key === "study_date" ? "desc" : "asc");
      memory.sort = key;
      run(0);
    },
  });

  /** The parameters of the search: the boxes, then the sort. */
  function searchParams() {
    const params = new URLSearchParams();
    for (const [name, input] of Object.entries(inputs)) {
      memory[name] = input.value;
      if (input.addParams) input.addParams(params);
      else if (input.value.trim()) params.set(name, input.value.trim());
    }
    if (memory.sort) { params.set("sort", memory.sort); params.set("dir", memory.dir || "asc"); }
    return params;
  }

  /** Calls the API with the current filters; `more` = the next results, added below the others. */
  async function run(newOffset, more = false) {
    offset = newOffset;
    const thisSearch = ++latestSearch;
    const params = searchParams();
    showSearchInAddress(cfg.id, params);
    params.set("limit", PAGE_SIZE);
    params.set("offset", offset);

    loading = true;
    if (!more) results.replaceChildren(h("div", { class: "loading" }, "Searching..."));
    try {
      const page = await api("GET", `${cfg.endpoint}?${params}`);
      // The user may have toggled another filter while this one was still loading:
      // answers can arrive out of order, and an old one must never replace a newer one.
      if (thisSearch !== latestSearch) return;
      hasMore = page.has_more;
      if (more) {
        results.querySelector("tbody").append(...page.items.flatMap((item) => cfg.rows(item)));
        shownCount += page.items.length;
        results.querySelector(".load-more").replaceWith(buildLoadMore());
      } else if (!page.items.length) {
        results.replaceChildren(unavailableNotice(page, () => run(0)) || "",
          h("div", { class: "empty" }, offset > 0 ? "No more results." : "No results. Try fewer filters, or a wildcard such as DOE*."));
      } else {
        shownCount = page.items.length;
        results.replaceChildren(unavailableNotice(page, () => run(0)) || "", cfg.renderTable(page.items, sorting()), options.infinite ? buildLoadMore() : buildPager(page));
      }
    } catch (e) {
      if (thisSearch !== latestSearch) return;
      if (!more) results.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    } finally {
      if (thisSearch === latestSearch) loading = false;
    }
  }

  /** "Showing 1-25   [Previous] [Next]". */
  function buildPager(page) {
    return h("div", { class: "pager" },
      h("span", { class: "muted" }, t("Showing {from}-{to}", { from: offset + 1, to: offset + page.items.length })),
      h("div", { class: "actions" },
        h("button", { class: "btn small", type: "button", disabled: offset === 0, onclick: () => run(Math.max(0, offset - PAGE_SIZE)) }, "Previous"),
        h("button", { class: "btn small", type: "button", disabled: !page.has_more, onclick: () => run(offset + PAGE_SIZE) }, "Next")));
  }

  /**
   * Endless list: the end of the list, which loads the next results when it scrolls into view
   * (or when its button is pressed).
   */
  function buildLoadMore() {
    const loadMore = () => { if (hasMore && !loading) run(offset + PAGE_SIZE, true); };
    const footer = h("div", { class: "pager load-more" },
      h("span", { class: "muted" }, t("{n} shown", { n: shownCount })),
      hasMore ? h("button", { class: "btn small", type: "button", onclick: loadMore }, "Load more") : h("span", { class: "muted" }, "That is all."));
    if (hasMore && "IntersectionObserver" in window) {
      const observer = new IntersectionObserver((entries) => {
        if (!footer.isConnected) { observer.disconnect(); return; }
        if (entries.some((e) => e.isIntersecting)) { observer.disconnect(); loadMore(); }
      }, { rootMargin: "300px" });
      setTimeout(() => observer.observe(footer), 0);
    }
    return footer;
  }

  const form = h("form", {
    class: "panel filters",
    onsubmit: (event) => { event.preventDefault(); run(0); },
  },
    h("div", { class: "filter-grid" }, serverNodes, fieldNodes),
    h("div", { class: "filter-actions" },
      h("button", { class: "btn primary", type: "submit" }, cfg.id === "series" ? "Search series" : "Search studies"),
      h("button", {
        class: "btn", type: "button",
        onclick: () => { for (const input of Object.values(inputs)) input.value = ""; delete memory.sort; delete memory.dir; run(0); },
      }, "Clear filters"),
      options.live ? h("span", { class: "muted" }, "Results update as you type.") : null));

  run(0); // show the most recent studies right away
  // After a delete / modify / anonymize, the list searches again (same filters, first page).
  const onChange = () => { if (results.isConnected) run(0); else window.removeEventListener("portal-archive-changed", onChange); };
  window.addEventListener("portal-archive-changed", onChange);
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, cfg.title),
      actionButton("button", "link", "Copy link", "Copy a link to this search (with its filters)", {
        class: "btn", type: "button",
        onclick: async () => toast((await copyText(location.href)) ? "Link to this search copied." : "Could not copy: copy the address of the page instead.", "info", 3000),
      })),
    form, results);
}

/** A column title; a button that sorts the list when the column can be sorted (`sorting` given). */
function columnHeader(title, sortKey, sorting, extraClass = null) {
  if (!sorting || !sortKey) return h("th", { class: extraClass }, title);
  const active = sorting.sort === sortKey;
  const arrow = active ? (sorting.dir === "desc" ? " ▼" : " ▲") : "";
  return h("th", { class: extraClass, "aria-sort": active ? (sorting.dir === "desc" ? "descending" : "ascending") : null },
    h("button", { type: "button", class: "sort-header", title: t("Sort by {column}", { column: lookup(title).toLowerCase() }), onclick: () => sorting.onSort(sortKey) }, title, arrow));
}

/**
 * Every column the studies tables can show; Settings > Search & labels chooses which ones, and in
 * which order. `sort` = the key the server sorts by (none = cannot be sorted).
 */
const STUDY_COLUMNS = {
  patient: { title: "Patient", sort: "patient", cell: (s) => {
    const demographics = [s.patient_sex, formatDicomDate(s.patient_birth_date)].filter(Boolean).join(", ");
    return h("td", null, h("div", { class: "patient" }, formatName(s.patient_name)), demographics ? h("div", { class: "sub" }, demographics) : null);
  } },
  patient_id: { title: "Patient ID", sort: "patient_id", cell: (s) => h("td", { class: "mono" }, s.patient_id || dash()) },
  birth_date: { title: "Birth date", sort: "birth_date", cell: (s) => h("td", { class: "nowrap" }, formatDicomDate(s.patient_birth_date) || dash()) },
  sex: { title: "Sex", sort: "sex", cell: (s) => h("td", null, s.patient_sex || dash()) },
  other_ids: { title: "Other patient IDs", sort: "other_ids", cell: (s) => h("td", { class: "mono" }, s.other_patient_ids || dash()) },
  study_date: { title: "Study date", sort: "study_date", cell: (s) => h("td", { class: "nowrap" }, formatDicomDate(s.study_date) || dash()) },
  description: { title: "Description", sort: "description", cell: (s) => h("td", null, s.study_description || dash()) },
  accession: { title: "Accession number", sort: "accession", cell: (s) => h("td", { class: "mono" }, s.accession_number || dash()) },
  referring: { title: "Referring physician", sort: "referring", cell: (s) => h("td", null, s.referring_physician ? formatName(s.referring_physician) : dash()) },
  requesting: { title: "Requesting physician", sort: "requesting", cell: (s) => h("td", null, s.requesting_physician ? formatName(s.requesting_physician) : dash()) },
  institution: { title: "Institution", sort: "institution", cell: (s) => h("td", null, s.institution || dash()) },
  modalities: { title: "Modality", cell: (s) => h("td", null, s.modalities.length ? s.modalities.map(chip) : dash()) },
  series: { title: "Series / images", cell: (s) => h("td", null, seriesCount(s.series_count), s.instances_count != null ? h("div", { class: "sub" }, imagesCount(s.instances_count)) : null) },
  labels: { title: "Labels", cell: (s) => h("td", { class: "labels-cell" }, labelChips(s.labels)) },
  server: { title: "Server", cell: (s) => h("td", null, serverBadges(s)) },
};
/** The fields the study window can show (Settings > Search & labels), and their titles. */
const STUDY_DETAIL_TITLES = {
  patient_id: "Patient ID", birth_date: "Birth date", sex: "Sex", other_ids: "Other patient IDs", study_date: "Study date",
  description: "Description", accession: "Accession number", referring: "Referring physician", requesting: "Requesting physician",
  institution: "Institution", modalities: "Modalities", series: "Content", labels: "Labels", study_uid: "StudyInstanceUID",
  server: "Stored on",
};
const DEFAULT_STUDY_COLUMNS = ["patient", "patient_id", "study_date", "description", "modalities", "series", "labels"];
const studyColumnKeys = () => [
  ...(searchOptions().columns || DEFAULT_STUDY_COLUMNS).filter((key) => STUDY_COLUMNS[key] && key !== "server" && (key !== "labels" || labelsOn())),
  // With several Orthanc servers, the last column says where each study is.
  ...(severalServers() ? ["server"] : []),
];
/** Number of cells in a study row: tick box, arrow, preview, the chosen columns, actions. */
const studyColumnCount = () => 2 + (bulkOn() ? 1 : 0) + (showThumbs() ? 1 : 0) + studyColumnKeys().length;

/** The studies table (Search, and the home page). `sorting` makes the column titles sort the list. */
function renderStudiesTable(studies, sorting = null) {
  const table = h("div", { class: "table-wrap" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null,
        bulkOn() ? h("th", { class: "pick-cell" }, pickAllBox()) : null,
        h("th", null, ""),
        showThumbs() ? h("th", null, "Preview") : null,
        studyColumnKeys().map((key) => columnHeader(STUDY_COLUMNS[key].title, STUDY_COLUMNS[key].sort, sorting)),
        h("th", { class: "num" }, "Actions"))),
      h("tbody", null, studies.map((s) => studyRows(s)))));
  if (!bulkOn()) return table;
  // With tick boxes: a bar above the table offers what can be done with the ticked studies.
  const block = h("div", { class: "studies-block" });
  block.append(buildBulkBar(block), table);
  return block;
}

/** Two table rows per study: the study itself and a hidden row that will hold its series. */
function studyRows(study) {
  const seriesCell = h("td", { colspan: studyColumnCount() }); // the whole width of the table
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
      noteViewed(study);
      seriesCell.replaceChildren(h("div", { class: "loading" }, "Loading series..."));
      try {
        seriesCell.replaceChildren(renderSeriesBox(await api("GET", `/api/studies/${study.id}/series`), study));
      } catch (e) {
        loaded = false; // allow a retry
        seriesCell.replaceChildren(h("div", { class: "series-empty muted" }, e.message));
      }
    },
  }, "▸");

  const cells = studyColumnKeys().map((key) => STUDY_COLUMNS[key].cell(study));
  // After a label is put on or taken off, only the labels cell is redrawn.
  const redrawLabels = () => {
    const i = studyColumnKeys().indexOf("labels");
    if (i >= 0) cells[i].replaceChildren(...[labelChips(study.labels)].flat());
  };
  const mainRow = h("tr", { class: "study" },
    bulkOn() ? h("td", { class: "pick-cell" }, pickBox(study)) : null,
    h("td", null, twisty),
    showThumbs() ? h("td", { class: "thumb-cell" }, thumb("studies", study.id, study.modalities[0])) : null,
    cells,
    h("td", null, h("div", { class: "actions" }, studyActions(study, redrawLabels, (copy) => {
      const [main, series] = studyRows(copyOf(study, copy));
      mainRow.replaceWith(main);
      seriesRow.replaceWith(series);
    }))));
  if (severalServers() && study.copies && study.copies.length > 1) mainRow.dataset.server = study.server;

  return [mainRow, seriesRow];
}

/** A study as its copy on another server: everything done with it then uses that server. */
const copyOf = (study, copy) => ({ ...study, id: copy.id, server: copy.server });

/**
 * "Open the copy on...": a study found on several servers (search results) acts on the copy of the
 * first server; this menu chooses another. `onCopy(copy)` redraws the study as that copy.
 */
function copiesMenu(study, onCopy) {
  if (!onCopy || !severalServers() || !study.copies || study.copies.length < 2) return null;
  return h("details", { class: "menu" },
    actionButton("summary", "servers", "Open the copy on...", t("Stored on {servers}: choose which copy to work with", { servers: study.copies.map((c) => serverName(c.server)).join(", ") }), { class: "btn small" }),
    h("div", { class: "menu-list" }, study.copies.map((copy) => {
      const current = copy.server === study.server;
      return h("button", {
        class: "menu-item", type: "button", "aria-current": current ? "true" : null, disabled: current,
        onclick: (event) => { closeMenu(event); onCopy(copy); },
      }, current ? t("{server} (this copy)", { server: serverName(copy.server) }) : t("Open the copy on {server}", { server: serverName(copy.server) }));
    })));
}

/**
 * Buttons shown for a study, according to the user's permissions. `onLabels` redraws after a label
 * change; `onCopy` (optional) redraws the study as its copy on another server.
 */
function studyActions(study, onLabels = () => {}, onCopy = null) {
  return [
    copiesMenu(study, onCopy),
    can("visualize") ? viewButtons(study) : null,
    study.patient_id || study.patient_name
      ? actionButton("button", "patient", "Other studies of this patient", "All the studies of this patient", { class: "btn small", type: "button", onclick: () => openPatientStudies(study) })
      : null,
    canEditLabels() ? actionButton("button", "tag", "Labels", "Labels: put labels on this study or take them off", { class: "btn small", type: "button", onclick: () => openLabelsDialog(study, onLabels) }) : null,
    can("send") ? actionButton("button", "send", "Send to...", "Send to a DICOM node", { class: "btn small", type: "button", onclick: () => openSendDialog(study) }) : null,
    can("share") ? actionButton("button", "share", "Share...", "Share with somebody outside (link + password)", { class: "btn small", type: "button", onclick: () => openShareDialog(study) }) : null,
    can("download") ? downloadMenu("studies", study.id) : null,
    can("visualize") && opts().instances && study.modalities.includes("DOC")
      ? actionButton("button", "pdf", "PDF report", "Open the PDF report of this study", { class: "btn small", type: "button", onclick: (e) => openStudyPdf(study, e.currentTarget) })
      : null,
    studyManageMenu(study),
  ];
}

/** Small table with the series of one study (shown under the study row). */
function renderSeriesBox(seriesList, study = null) {
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
        h("td", null, h("div", { class: "actions" }, seriesActions(s, study))))))));
}

/** Buttons of one series: its images and tags, the slide viewer, download, manage. */
function seriesActions(s, study) {
  return [
    opts().instances && can("read") ? actionButton("button", "images", "Images", "The images of this series, their DICOM tags (and PDF reports)", { class: "btn small", type: "button", onclick: () => openInstancesDialog(s) }) : null,
    can("visualize") && viewersOpt().wsi && s.modality === "SM"
      ? actionButton("a", "viewers", "Slide viewer", "Open in the whole-slide viewer (microscopy)", { class: "btn small", href: `/wsi/app/viewer.html?series=${s.id}`, target: "_blank", rel: "noopener" }) : null,
    can("download") ? downloadMenu("series", s.id) : null,
    seriesManageMenu(s, study),
  ];
}


/** The columns of the Series list: title and the key the server sorts by. */
const SERIES_COLUMNS = [["Patient", "patient"], ["Study date", "study_date"], ["Study", null], ["#", "number"], ["Modality", "modality"],
  ["Series description", "description"], ["Body part", "body_part"], ["Images", null]];

/** Results table of the Series search. Each series shows the patient/study it belongs to. */
function renderSeriesTable(seriesList, sorting = null) {
  return h("div", { class: "table-wrap" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null,
        showThumbs() ? h("th", null, "Preview") : null,
        SERIES_COLUMNS.map(([title, key]) => columnHeader(title, key, sorting, title === "Images" ? "num" : null)),
        severalServers() ? h("th", null, "Server") : null,
        h("th", { class: "num" }, "Actions"))),
      h("tbody", null, seriesList.map(seriesRow))));
}

/** One row of the Series list. */
function seriesRow(s) {
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
    severalServers() ? h("td", null, serverBadges(s)) : null,
    h("td", null, h("div", { class: "actions" }, [
      can("visualize") && study ? viewButtons(study) : null,
      study ? actionButton("button", "patient", "Other studies of this patient", "All the studies of this patient", { class: "btn small", type: "button", onclick: () => openPatientStudies(study) }) : null,
      opts().instances ? actionButton("button", "images", "Images", "The images of this series and their DICOM tags", { class: "btn small", type: "button", onclick: () => openInstancesDialog(s) }) : null,
      can("download") ? downloadMenu("series", s.id) : null,
      seriesManageMenu(s, study),
    ])));
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
async function openInWeasis(studyUid, button, server = "") {
  if (!studyUid) { toast("This study has no StudyInstanceUID.", "error"); return; }
  button.disabled = true;
  try {
    const link = await api("GET", `/api/weasis/link?study_uid=${encodeURIComponent(studyUid)}${serverParam(server)}`);
    toast("Opening Weasis. If nothing happens, check that Weasis is installed on this computer.", "info", 8000);
    window.location.href = link.uri;
  } catch (e) {
    toastError(e);
  } finally {
    button.disabled = false;
  }
}

/** Address of the OHIF web viewer for a study (served by Orthanc's OHIF plugin through the portal). */
const ohifUrl = (studyUid, server = "") => `/ohif/viewer?StudyInstanceUIDs=${encodeURIComponent(studyUid)}${serverParam(server)}`;

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
    weasis.addEventListener("click", () => { noteViewed(study); openInWeasis(study.study_instance_uid, weasis, study.server); });
    buttons.push(weasis);
  }
  if (hasOhif() && (viewersOpt().ohif_modes || ["viewer"]).includes("viewer")) {
    buttons.push(actionButton("a", "ohif", "OHIF", "Open in the browser viewer (OHIF): new tab, nothing to install", {
      class: "btn small", href: ohifUrl(study.study_instance_uid, study.server), target: "_blank", rel: "noopener",
      title: "Open in the browser viewer (OHIF): new tab, nothing to install", onclick: () => noteViewed(study),
    }));
  }
  const more = moreViewersMenu([study]);
  if (more) {
    more.addEventListener("click", (event) => { if (event.target.closest("a")) noteViewed(study); });
    buttons.push(more);
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
      h("a", { class: "menu-item", href: `/api/${kind}/${id}/download?dicomdir=true`, onclick: closeMenu }, "ZIP with DICOMDIR", h("small", null, "Adds an index, like a CD/DVD")),
      kind === "series" && opts().nifti ? h("a", { class: "menu-item", href: `/api/series/${id}/nifti`, onclick: closeMenu }, "NIfTI", h("small", null, "One 3D volume file (.nii.gz)")) : null,
      kind === "studies" ? cdExportItem([id]) : null));
}

/** "Patient CD / USB" of the download menus: the images with their index, a page for the patient and a viewer. */
function cdExportItem(ids) {
  if (!opts().cd_export) return null;
  return h("a", {
    class: "menu-item", href: `/api/export/cd?studies=${ids.join(",")}`,
    onclick: (event) => { closeMenu(event); toast("Preparing the patient CD / USB: the download starts when it is ready (large studies take a while).", "info", 7000); },
  }, "Patient CD / USB", h("small", null, "To copy onto a disc or a stick: index, a page for the patient, a viewer"));
}

/**
 * Dialog to send a whole study to a remote DICOM node (an Orthanc "modality").
 * The transfer runs as an Orthanc job; follow it in the Transfers tab.
 */
async function openSendDialog(studyOrList) {
  const studies = [].concat(studyOrList);
  const study = studies[0];
  let nodes;
  try {
    nodes = await api("GET", "/api/modalities");
  } catch (e) { toastError(e); return; }
  if (!nodes.length) {
    toast("No DICOM destination is configured in Orthanc (DicomModalities).", "error", 8000);
    return;
  }

  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const kindName = { peer: "[Orthanc peer]", dicomweb: "[DICOMweb]" };
  const target = h("select", null, nodes.map((n) =>
    h("option", { value: n.id }, [n.description ? `${n.description}: ${n.id}` : n.id,
      n.aet ? `  ${t("({aet} at {address})", { aet: n.aet, address: `${n.host}:${n.port}` })}` : n.url ? `  (${n.url})` : "",
      kindName[n.kind] ? ` ${t(kindName[n.kind])}` : ""].join(""))));
  const submit = h("button", { class: "btn primary", type: "submit" }, studies.length > 1 ? t("Send {n} studies", { n: studies.length }) : "Send study");

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      submit.disabled = true;
      try {
        if (studies.length > 1) {
          const r = await api("POST", "/api/send", { target: target.value, studies: studies.map((s) => s.id) });
          if (r.errors.length) toast(t("{n} could not be sent: {error}", { n: r.errors.length, error: translateMessage(r.errors[0]) }), "error", 10000);
        } else {
          await api("POST", `/api/studies/${study.id}/send`, { target: target.value });
        }
        dialog.close();
        toast(t("Transfer to {target} started. Follow it on the Transfers page.", { target: target.value }), "info", 7000);
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
      }
    },
  },
    h("h2", null, studies.length > 1 ? t("Send {n} studies", { n: studies.length }) : "Send study to a DICOM destination"),
    studies.length > 1
      ? h("ul", null, studies.slice(0, 8).map((s) => h("li", null, studyLine(s))), studies.length > 8 ? h("li", null, t("and {n} more", { n: studies.length - 8 })) : null)
      : h("p", null, h("b", null, formatName(study.patient_name)), " ", formatDicomDate(study.study_date), study.study_description ? `, ${study.study_description}` : ""),
    error,
    h("label", null, "Destination", target),
    h("p", { class: "muted" }, "Orthanc sends every image to the chosen destination (DICOM C-STORE, or the Orthanc / DICOMweb protocol). Each study is one transfer."),
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
  if (folder && files.every((f) => (f.webkitRelativePath || "").startsWith(`${folder}/`))) return t("Folder {name}", { name: folder });
  return files.length === 1 ? files[0].name : t("{n} files", { n: files.length });
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
    // The new studies appear on the home page and in the total at once.
    if (upload.uploaded > 0) {
      if (reloadRecentStudies) reloadRecentStudies();
      refreshTotalStudies(true);
    }
    toast(t("Upload finished: {new} new, {duplicates} already stored, {failed} failed.", { new: upload.uploaded, duplicates: upload.duplicates, failed: upload.failed }),
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
        failFile(item, translateMessage(data && data.error) || `HTTP ${xhr.status}`);
        if (xhr.status === 401) { upload.pending = []; upload.next = 0; sessionExpired(); } // stop: we are signed out
      }
      fileDone(item);
      resolve();
    };
    xhr.onerror = () => { failFile(item, t("Network error")); fileDone(item); resolve(); };
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
    if (!running) detail = t("{size} in {time}", { size: formatBytes(b.bytesTotal), time: formatDuration(Math.max(1, seconds)) });
    else if (!started) detail = "Waiting for the uploads before it";
    else {
      const speed = sent / Math.max(seconds, 0.5);   // bytes per second since it started
      const left = speed > 0 ? (b.bytesTotal - sent) / speed : 0;
      detail = t("{sent} of {total} · {speed}/s · about {time} left", { sent: formatBytes(sent), total: formatBytes(b.bytesTotal), speed: formatBytes(speed), time: formatDuration(Math.max(1, left)) });
    }
    const fill = h("span");
    fill.style.width = `${Math.round(fraction * 100)}%`;
    const [stateClass, stateText] = running ? (started ? ["state-running", "Uploading"] : ["state-pending", "Waiting"])
      : b.failed ? ["state-failure", b.failed === b.total ? "Failed" : "Done with errors"] : ["state-success", "Done"];
    return h("tr", null,
      h("td", { class: "nowrap" }, new Date(b.started).toLocaleTimeString(i18n.locale)),
      h("td", null, h("div", { class: "patient" }, b.label), h("div", { class: "sub" }, t(b.total === 1 ? "{n} file, {size}" : "{n} files, {size}", { n: b.total, size: formatBytes(b.bytesTotal) }))),
      h("td", { class: "upload-progress-cell" },
        h("div", { class: "upload-progress-line" }, h("div", { class: "progress" }, fill), h("b", null, `${Math.floor(fraction * 100)}%`)),
        h("div", { class: "sub" }, detail)),
      h("td", { class: "num nowrap" }, t("{done} of {total}", { done, total: b.total })),
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

  ui.statDone.textContent = t("{done} of {total}", { done, total: upload.total });
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
  const label = entries.length === 1 ? (entries[0].isDirectory ? t("Folder {name}", { name: entries[0].name }) : entries[0].name) : t("{n} items", { n: entries.length });

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
 * `home` = the compact version of the home page.
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

  let polling = null; // the quick follow-up of a running transfer
  async function load() {
    clearTimeout(polling);
    try {
      await nodesLoaded;
      const rows = await api("GET", "/api/transfers");
      container.replaceChildren(rows.length ? renderTransfers(rows) : h("div", { class: "empty" }, 'Nothing sent yet. Use "Send to..." on a study.'));
      // Follow a transfer closely while it is still in progress.
      if (rows.some((r) => ["Pending", "Running", "Retry"].includes(r.state))) {
        polling = setTimeout(() => { if (document.body.contains(container)) load(); }, 3000);
      }
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }

  /** The destination: its short name, with its description and address in a tooltip and under it. */
  function destinationCell(alias) {
    const n = nodes.get(alias);
    const address = n && n.aet ? t("AE title {aet} at {address}", { aet: n.aet, address: `${n.host}:${n.port}` }) : "";
    const tip = n
      ? [n.description, address].filter(Boolean).join(" — ") || alias
      : t("{alias}: no longer among the DICOM destinations", { alias });
    return h("td", null,
      h("span", { class: "chip has-tip", "data-tip": tip, "aria-label": tip.startsWith(alias) ? tip : `${alias}: ${tip}`, tabindex: "0" }, alias),
      n && n.description ? h("div", { class: "sub" }, n.description) : null);
  }

  /** "45%, 54 of 120 images, 1 min 10 s, about 1 min 25 s left" while it runs; the outcome when finished. */
  function progressCell(r) {
    const fill = h("span");
    fill.style.width = `${r.progress}%`;
    const images = r.instances;
    const ran = r.runtime ? formatDuration(r.runtime) : "";
    // The same words for both directions: sent to another system, or retrieved from it.
    const retrieved = r.direction === "retrieve";
    let detail;
    if (r.state === "Pending") detail = "Waiting to start";
    else if (r.state === "Running" || r.state === "Retry") {
      const parts = [];
      if (images) parts.push(t("{done} of {total} images", { done: Math.round((images * r.progress) / 100), total: images }));
      if (ran) parts.push(ran);
      if (r.runtime && r.progress > 0 && r.progress < 100) parts.push(t("about {time} left", { time: formatDuration((r.runtime * (100 - r.progress)) / r.progress) }));
      detail = parts.join(" · ") || (retrieved ? "Retrieving" : "Sending");
    } else if (r.state === "Success") {
      detail = images
        ? t(retrieved ? "{n} images retrieved in {time}" : "{n} images sent in {time}", { n: images, time: ran || t("a moment") })
        : (retrieved ? "Retrieved" : "Sent");
    } else if (r.state === "Failure") {
      detail = images && r.failed
        ? t(retrieved ? "{failed} of {total} images could not be retrieved" : "{failed} of {total} images could not be sent", { failed: r.failed, total: images })
        : "";
    }
    else if (r.state === "Unknown") detail = "Orthanc no longer remembers this transfer";
    else if (r.state === "Unavailable") detail = "Orthanc did not answer: trying again";
    const running = ["Pending", "Running", "Retry"].includes(r.state);
    return h("td", { class: "transfer-progress" },
      running ? h("div", { class: "upload-progress-line" }, h("div", { class: "progress" }, fill), h("b", null, `${r.progress}%`)) : null,
      detail ? h("div", { class: "sub" }, detail) : null,
      r.error ? h("div", { class: "error-line" }, translateMessage(r.error)) : null);
  }

  function renderTransfers(rows) {
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Started", "Study", "Destination", "By", "Progress", "State"].map((t) => h("th", null, t)))),
      h("tbody", null, rows.map((r) => h("tr", null,
        h("td", { class: "nowrap" }, formatTimestamp(r.ts)),
        h("td", null, r.label.replaceAll(" | ", " - "),
          r.direction === "retrieve" && r.state === "Success" && r.study_uid
            ? h("div", null, h("button", { class: "btn small", type: "button", onclick: () => openSearchWith("studies", { study_uid: r.study_uid }) }, "Open the study"))
            : null),
        r.direction === "retrieve"
          ? h("td", null, h("span", { class: "chip" }, "from"), " ", destinationCell(r.target).firstChild || r.target)
          : destinationCell(r.target),
        h("td", null, r.username),
        progressCell(r),
        h("td", null, h("span", { class: `chip state-${r.state.toLowerCase()}` }, r.state)))))));
  }

  load();
  const transfersAuto = simpleOptions().transfers_auto_refresh ? autoRefresh("transfers", container, load) : null;
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Transfers"),
      h("div", { class: "head-actions" },
        transfersAuto,
        actionButton("button", "refresh", "Refresh", "Refresh the list", {
          class: "btn", type: "button", onclick: () => { load(); if (transfersAuto) transfersAuto.restart(); },
        }))),
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
    const servers = (meta.orthanc_servers || []).length > 1;
    const columns = ["User", "Role", "Access", ...(servers ? ["Servers"] : []), "Status", "Last sign-in", "Actions"];
    return h("div", { class: "table-wrap" },
      h("table", { class: "data" },
        h("thead", null, h("tr", null, columns.map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
        h("tbody", null, meta.users.map((u) => h("tr", null,
          h("td", null, h("span", { class: "patient" }, u.username), u.id === state.user.id ? h("span", { class: "sub" }, " (you)") : null),
          h("td", null, roleLabel(u.effective_role)),
          h("td", null, u.permissions.length ? u.permissions.map((p) => h("span", { class: "chip perm" }, p)) : h("span", { class: "muted" }, "no access")),
          servers ? h("td", null, accountServers(u, meta)) : null,
          h("td", null, h("span", { class: `chip ${u.active ? "status-on" : "status-off"}` }, u.active ? "Active" : "Disabled"),
            u.two_factor ? [" ", h("span", { class: "chip", title: "Signs in with a code from a phone app too" }, "two-step")] : null,
            u.email ? h("div", { class: "sub" }, u.email) : null),
          h("td", null, formatTimestamp(u.last_login)),
          h("td", null, h("div", { class: "actions" },
            h("button", { class: "btn small", type: "button", onclick: () => openUserDialog(u, meta, load) }, "Edit"),
            h("button", {
              class: "btn small danger", type: "button", disabled: u.id === state.user.id,
              onclick: async () => {
                if (!confirm(t('Delete the user "{user}"? This cannot be undone.', { user: u.username }))) return;
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

/** The servers column of the users list: "All", or the names of the account's servers. */
function accountServers(user, meta) {
  if (user.permissions.includes("admin") || user.servers == null) return h("span", { class: "muted" }, "All");
  if (!user.servers.length) return h("span", { class: "muted" }, "none");
  const name = (key) => ((meta.orthanc_servers || []).find((x) => x.key === key) || { label: key }).label;
  return user.servers.map((key) => h("span", { class: "chip" }, name(key)));
}

/**
 * "Servers this account can use: (o) All ( ) Only: [x] Main [ ] Archive". `initial` = null (all) or
 * a list of keys. `.value` = null or the ticked keys. Hidden with a single server.
 */
function buildServerAccess(servers, initial, legend = "Servers this account can use") {
  const name = `servers-${Math.random().toString(36).slice(2)}`;
  const allRadio = h("input", { type: "radio", name, checked: initial == null });
  const onlyRadio = h("input", { type: "radio", name, checked: initial != null });
  const boxes = servers.map((server) => ({
    key: server.key,
    box: h("input", { type: "checkbox", checked: initial != null && initial.includes(server.key) }),
    server,
  }));
  const list = h("div", { class: "role-perms" }, boxes.map(({ box, server }) => h("label", { class: "check" }, box,
    server.label, server.enabled === false ? h("small", null, " ", lookup("(switched off)")) : null)));
  const sync = () => { list.hidden = !onlyRadio.checked; };
  allRadio.addEventListener("change", sync);
  onlyRadio.addEventListener("change", sync);
  sync();
  const node = h("fieldset", { class: "perm-list server-access" }, h("legend", null, legend),
    h("label", { class: "check" }, allRadio, "All servers"),
    h("label", { class: "check" }, onlyRadio, "Only these:"),
    list);
  return {
    node,
    get value() { return allRadio.checked ? null : boxes.filter((b) => b.box.checked).map((b) => b.key); },
    set value(keys) {
      allRadio.checked = keys == null;
      onlyRadio.checked = keys != null;
      for (const b of boxes) b.box.checked = keys != null && keys.includes(b.key);
      sync();
    },
  };
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
    // Users follow their role, so ticking by hand makes the account the role that matches, or Custom.
    box.addEventListener("change", () => { preset.value = matchingRole(); });
    checks[p.key] = box;
    return h("label", null, box, h("span", null, p.key, h("small", null, p.description)));
  });

  // The role is saved with the account (shown in the users list). Choosing one ticks its permissions.
  const initialRole = user ? user.effective_role || "" : (meta.presets.some((p) => p.key === "viewer") ? "viewer" : "");
  /** The role whose permissions are exactly the ticked ones: the current one first, else the first; "" = Custom. */
  function matchingRole() {
    const ticked = Object.keys(checks).filter((key) => checks[key].checked).sort().join(",");
    const same = (p) => [...p.permissions].sort().join(",") === ticked;
    const current = meta.presets.find((p) => p.key === preset.value);
    return current && same(current) ? current.key : (meta.presets.find(same) || { key: "" }).key;
  }
  // With several Orthanc servers: which ones the account may use (a role gives its own).
  const serverAccess = (meta.orthanc_servers || []).length > 1
    ? buildServerAccess(meta.orthanc_servers, user ? user.servers : (meta.presets.find((p) => p.key === initialRole) || {}).servers ?? null)
    : null;
  const preset = h("select", {
    onchange: (event) => {
      const chosen = meta.presets.find((p) => p.key === event.target.value);
      if (chosen) for (const key of Object.keys(checks)) checks[key].checked = chosen.permissions.includes(key);
      if (chosen && serverAccess) serverAccess.value = chosen.servers ?? null;
    },
  }, h("option", { value: "", selected: !initialRole }, "Custom (no role)"),
  meta.presets.map((p) => h("option", { value: p.key, selected: p.key === initialRole }, p.label)));

  const active = h("input", { type: "checkbox", checked: user ? user.active : true });
  const email = h("input", { name: "email", type: "email", autocomplete: "off", value: user ? user.email || "" : "", placeholder: "Optional: for notifications of their links" });
  const resetTwoFactor = h("input", { type: "checkbox" });

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      const permissions = Object.keys(checks).filter((key) => checks[key].checked);
      const servers = serverAccess ? { servers: serverAccess.value } : {};
      if (serverAccess && servers.servers && !servers.servers.length) {
        error.textContent = lookup("Tick at least one server, or choose All servers.");
        error.hidden = false;
        return;
      }
      try {
        if (isNew) {
          await api("POST", "/api/admin/users", { username: username.value.trim(), password: password.value, permissions, role: preset.value, email: email.value, ...servers });
        } else {
          const body = { permissions, active: active.checked, role: preset.value, email: email.value, ...servers };
          if (resetTwoFactor.checked) body.reset_two_factor = true;
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
    h("h2", null, isNew ? "New user" : t("Edit {name}", { name: user.username })),
    error,
    h("label", null, "Username", username),
    h("label", null, isNew ? "Password" : "New password (optional)", password),
    h("label", null, "Email address", email),
    user && user.two_factor ? h("label", { class: "check" }, resetTwoFactor, "Reset the two-step sign-in (lost phone): the next sign-in needs only the password, or sets it up again if it is required") : null,
    h("label", null, "Role", preset),
    h("p", { class: "hint" }, "Choosing a role ticks its permissions. The user then follows the role: when the role is changed in Settings > Roles, this user changes too. Ticking other permissions by hand makes the account Custom."),
    h("fieldset", { class: "perm-list" }, h("legend", null, "Permissions"), permissionBoxes),
    serverAccess ? serverAccess.node : null,
    serverAccess ? h("p", { class: "hint" }, "The studies of the other servers do not exist for this account: they are not found, not shown and cannot be opened. Administrators can always use every server.") : null,
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
  // While a modal <dialog> is open, everything outside it is inert and cannot be selected,
  // so the helper textarea must go inside that dialog (the share window, for example).
  const returnFocus = document.activeElement;
  const host = returnFocus?.closest?.("dialog[open]") || document.querySelector("dialog[open]") || document.body;
  const area = h("textarea", { readonly: true }, text);
  area.style.position = "fixed";
  area.style.top = "0";
  area.style.left = "0";
  area.style.opacity = "0";
  host.append(area);
  area.focus();
  area.select();
  area.setSelectionRange(0, text.length);
  let copied = false;
  try { copied = document.execCommand("copy"); } catch { copied = false; }
  area.remove();
  returnFocus?.focus?.();
  return copied;
}

/** A button that copies `getText()` and says so. */
function copyButton(label, getText, iconName = "") {
  const props = {
    class: "btn small", type: "button",
    onclick: async () => toast((await copyText(getText())) ? "Copied." : "Could not copy: select the text and copy it by hand.", "info", 2500),
  };
  // An icon when the caller gives one (the Shares list); text everywhere else (dialogs).
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
  const part = (n, one, many) => (n ? `${n} ${t(n === 1 ? one : many)}` : "");
  return [part(d, "day", "days"), part(hh, "hour", "hours"), part(mm, "minute", "minutes")].filter(Boolean).join(" ") || t("0 minutes");
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
    node: h("fieldset", { class: "duration" }, h("legend", null, t("{what} (at most {duration})", { what: lookup(legend), duration: durationWords(maxMinutes) })),
      part(days, "days"), part(hours, "hours"), part(minutes, "minutes")),
    get minutes() { return total(); },
    problem() {
      if (total() < 1) return t("Choose how long the link works: at least 1 minute.");
      if (total() > maxMinutes) return t("A link can work for at most {duration}.", { duration: durationWords(maxMinutes) });
      return "";
    },
  };
}

/**
 * "Send the invitation by email" of the Share windows. Unticked, unless `newLink` and an administrator
 * chose to tick it by default (Settings > Share links). `.value` is null, or the `email` object the
 * server expects. `canIncludePassword()` says whether the portal has the password.
 */
function buildEmailOptions(canIncludePassword, newLink = false) {
  const feature = state.user.features.share;
  if (!feature.email) {
    return {
      node: h("p", { class: "hint" }, "To send invitations by email, an administrator must first set up the mail server in Settings > Network & sharing > Email."),
      value: null, refresh() {},
    };
  }
  const send = h("input", { type: "checkbox" });
  send.checked = newLink && !!feature.email_default;
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
    passwordHint.textContent = t(!possible
      ? "This link's password is not kept, so it cannot go in the email: tick \"Give the link a new password\" to include one."
      : withPassword.checked
        ? "Anyone who can read this email can open the study. Safer: leave it out and give the password by phone or text message."
        : "The email says the password will be given separately (by phone or text message): safer.");
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
    ? h("p", { class: "notice" }, t("Invitation emailed to {to}.", { to: email.to }))
    : h("p", { class: "form-error" }, t("The link is ready, but the email to {to} was not sent: {error}", { to: email.to, error: translateMessage(email.error) }));
}

/**
 * Dialog to invite somebody outside to look at a study (the "Share..." button of a study), or several.
 * Kept short: how long, how many downloads, a note, email; the anonymized copy and "email me" are under
 * "More options". `how` comes from the Shares page (advanced sharing): `{ patient: true }` = every study
 * of the patient of `studyOrList[0]`, `{ label }` = every study with that label (the server finds them).
 */
function openShareDialog(studyOrList, how = null) {
  const studies = [].concat(studyOrList);
  const study = studies[0];
  const several = studies.length > 1;
  const feature = state.user.features.share;
  const dialog = h("dialog", { class: "share-dialog" });
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  const close = h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close");
  const who = several
    ? h("ul", null, studies.slice(0, 8).map((s) => h("li", null, studyLine(s))), studies.length > 8 ? h("li", null, t("and {n} more", { n: studies.length - 8 })) : null)
    : h("p", null, h("b", null, formatName(study.patient_name)), " ", formatDicomDate(study.study_date), study.study_description ? `, ${study.study_description}` : "");

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
    const emailOptions = buildEmailOptions(() => true, true);
    const note = h("input", { maxlength: 120, placeholder: "For your own records, e.g. Dr. Smith, second opinion", autocomplete: "off" });
    const submit = h("button", { class: "btn primary", type: "submit" }, "Create link and password");
    // Less common options: an anonymized copy, tell me when it is used.
    const anonymize = h("input", { type: "checkbox" });
    const anonymizeOption = feature.anonymize ? h("div", null,
      h("label", { class: "check" }, anonymize, "Share an anonymized copy"),
      h("p", { class: "hint" }, "The person sees the images without the patient's name and ID (ANONYMOUS). The copy is made now and deleted when the link is revoked or long after it expires; the original is not changed.")) : null;
    const notify = h("input", { type: "checkbox", disabled: !state.user.email });
    const notifyOption = feature.notify ? h("div", null,
      h("label", { class: "check" }, notify, "Email me when the link is opened or downloaded"),
      state.user.email
        ? h("p", { class: "hint" }, t("To {email}. The email names the link, never the patient.", { email: state.user.email }))
        : h("p", { class: "hint" }, "Add your email address in My account first.")) : null;
    dialog.replaceChildren(h("form", {
      class: "dialog-form",
      onsubmit: async (event) => {
        event.preventDefault();
        error.hidden = true;
        const problem = duration.problem();
        if (problem) { error.textContent = problem; error.hidden = false; return; }
        submit.disabled = true;
        if (anonymize.checked) submit.textContent = t("Making the anonymized copy...");
        try {
          const body = {
            minutes: duration.minutes, downloads: Number(downloads.value), note: note.value, email: emailOptions.value,
            anonymize: anonymize.checked, notify: notify.checked,
          };
          let result;
          if (how && how.label) result = await api("POST", "/api/shares", { ...body, label: how.label });
          else if (how && how.patient) result = await api("POST", `/api/studies/${study.id}/share`, { ...body, patient: true });
          else if (several) result = await api("POST", "/api/shares", { ...body, studies: studies.map((s) => s.id) });
          else result = await api("POST", `/api/studies/${study.id}/share`, body);
          showResult(result);
        } catch (e) {
          error.textContent = e.message;
          error.hidden = false;
          submit.disabled = false;
          submit.textContent = t("Create link and password");
        }
      },
    },
      h("h2", null, how && how.label ? t("Share every study with the label {label} (one link)", { label: how.label })
        : how && how.patient ? t("Share every study of {name} (one link)", { name: formatName(study.patient_name) })
          : several ? t("Share {n} studies with somebody outside (one link)", { n: studies.length }) : "Share this study with somebody outside"),
      who, error,
      duration.node,
      h("label", null, t("Downloads allowed as ZIP (0 = online viewing only, up to {max})", { max: feature.max_downloads }), downloads),
      h("label", null, "Note (optional)", note),
      emailOptions.node,
      // Advanced sharing (from the Shares page) shows the options; a plain share keeps them folded away.
      anonymizeOption || notifyOption
        ? (how ? [anonymizeOption, notifyOption] : h("details", { class: "more-options" }, h("summary", null, "More options"), anonymizeOption, notifyOption))
        : null,
      h("p", { class: "muted" }, several || how
        ? "The person gets a link and a password that opens only these studies. They can look at them online until the link expires; running out of downloads does not stop that."
        : "The person gets a link and a password that opens only this study. They can look at it online until the link expires; running out of downloads does not stop that."),
      !how && (feature.patient || feature.label) && can("read")
        ? h("p", { class: "hint" }, "To share every study of a patient, or every study with a label, use \"New share...\" on the Shares page.") : null,
      h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), submit)));
  }

  /** Step 2: the link and the password. */
  function showResult(result) {
    const expires = new Date(result.expires_at * 1000).toLocaleString(i18n.locale);
    const link = h("input", { readonly: true, value: result.url, "aria-label": "Link", onfocus: (e) => e.target.select() });
    const invitation = () => t("You have been invited to look at an imaging study.\nLink: {url}\nThe link works until {expires}.\nThe password is sent to you separately.", { url: result.url, expires });
    dialog.replaceChildren(h("div", { class: "dialog-form" },
      h("h2", null, "Link and password created"),
      who,
      h("label", null, "Link", h("div", { class: "secret-box" }, link, copyButton("Copy link", () => result.url))),
      h("div", null, h("div", { class: "muted" }, "Password"),
        h("div", { class: "secret-box" }, h("span", { class: "password" }, result.password), copyButton("Copy password", () => result.password))),
      result.password_kept
        ? h("p", { class: "muted" }, "Give the password to the person by a different channel than the link (a phone call, an SMS). You can see it again in the Shares list while the link is active.")
        : h("p", { class: "form-error" }, "The password is shown only now and cannot be recovered. Give it to the person by a different channel than the link (a phone call, an SMS)."),
      h("p", { class: "muted" }, t("Valid until {expires}. Downloads allowed: {n}.", { expires, n: result.max_downloads }),
        result.studies > 1 ? ` ${t("{n} studies in this link.", { n: result.studies })}` : "",
        result.anonymized ? ` ${t("The person sees an anonymized copy.")}` : ""),
      feature.qr ? qrBlock(`/api/shares/${result.id}/qr`, "link-qr-code.svg") : null,
      emailOutcome(result.email),
      h("div", { class: "dialog-actions" }, copyButton("Copy invitation text (without the password)", invitation), close)));
  }

  showForm();
  dialog.showModal();
  return dialog;
}

/** The QR code of a link (to print on a handout: the phone's camera opens it) and a download of it. */
function qrBlock(url, fileName) {
  return h("div", { class: "qr-block" },
    h("img", { class: "qr", src: url, alt: t("QR code of the link"), width: 160, height: 160 }),
    h("div", null,
      h("p", { class: "muted" }, "The QR code opens the link with a phone's camera: handy on a printed handout. It does not contain the password."),
      h("a", { class: "btn small", href: url, download: fileName }, "Download the QR code")));
}

/** A dialog with the QR code of a link (Shares list). */
function openQrDialog(url, title) {
  const dialog = h("dialog", null, h("div", { class: "dialog-form" },
    h("h2", null, title), qrBlock(url, "link-qr-code.svg"),
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
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

/** The name of a link: "DOE JOHN - 20240517 - CT", with "+ 2 more" for a link to several studies. */
const shareLabel = (label) => label.replaceAll(" | ", " - ").replace(/ \+ (\d+) more$/, (all, n) => ` ${t("+ {n} more", { n })}`);

/** A small dialog with a link's password and a copy button. */
function passwordDialog(share, title, password, message) {
  const dialog = h("dialog", null, h("div", { class: "dialog-form" },
    h("h2", null, title),
    h("p", null, shareLabel(share.label), share.note ? h("span", { class: "muted" }, ` (${share.note})`) : null),
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
          if (r.email.sent) toast(t("Invitation emailed to {to}.", { to: r.email.to }));
          else toast(t("The link was updated, but the email was not sent: {error}", { error: translateMessage(r.email.error) }), "error", 10000);
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
    h("p", null, h("b", null, shareLabel(share.label))),
    h("p", { class: "muted" }, active
      ? t("It works for {time} more (until {until}). {used} of {max} downloads used.", { time: formatDuration(share.expires_at - now), until: formatTimestamp(share.expires_at), used: share.downloads_used, max: share.max_downloads })
      : t("It ended on {date}: saving makes it work again. {used} of {max} downloads used.", { date: formatTimestamp(share.expires_at), used: share.downloads_used, max: share.max_downloads })),
    error,
    duration.node,
    h("label", null, t("Downloads left from now on (0 = online viewing only, up to {max})", { max: feature.max_downloads }), left),
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
        h("td", null, shareLabel(s.label), s.note ? h("div", { class: "sub" }, s.note) : null,
          s.anonymized || s.notify ? h("div", null,
            s.anonymized ? h("span", { class: "chip" }, "anonymized copy") : null, " ",
            s.notify ? h("span", { class: "chip" }, "emails me") : null) : null),
        h("td", null, s.created_by),
        h("td", { class: "nowrap" }, formatTimestamp(s.created_at)),
        timeLeftCell(s, now),
        downloadsCell(s),
        h("td", { class: "nowrap" }, s.access_count ? t("{n}x, last {date}", { n: s.access_count, date: formatTimestamp(s.last_access_at) }) : "never"),
        h("td", null, h("span", { class: `chip ${s.status === "active" ? "status-on" : "status-off"}` }, s.status)),
        h("td", null, h("div", { class: "actions" },
          s.url && s.status === "active" ? copyButton("Copy link", () => s.url, "link") : null,
          s.url && s.status === "active" && state.user.features.share.qr
            ? actionButton("button", "qr", "QR code", "The QR code of the link (to print)", { class: "btn small", type: "button", onclick: () => openQrDialog(`/api/shares/${s.id}/qr`, t("QR code of the link")) })
            : null,
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
                  if (!confirm(t("Revoke this link? The person will not be able to open it any more."))) return;
                  try { await api("POST", `/api/shares/${s.id}/revoke`); toast("Link revoked."); load(); } catch (e) { toastError(e); }
                },
              })
            : null)))))));
  }

  /** How long the link still works, and until when (amber in its last hour). */
  function timeLeftCell(s, now) {
    const until = t("until {date}", { date: formatTimestamp(s.expires_at) });
    if (s.status === "revoked") return h("td", { class: "nowrap muted" }, "withdrawn");
    if (s.status !== "active") return h("td", { class: "nowrap" }, h("span", { class: "muted" }, "ended"), h("div", { class: "sub" }, formatTimestamp(s.expires_at)));
    const left = s.expires_at - now;
    return h("td", { class: "nowrap" }, h("span", { class: left < 3600 ? "soon" : null }, formatDuration(left)), h("div", { class: "sub" }, until));
  }

  /** Downloads still allowed, and how many were used (amber when none are left). */
  function downloadsCell(s) {
    if (!s.max_downloads) return h("td", { class: "nowrap" }, "view only", h("div", { class: "sub" }, "no download allowed"));
    // A withdrawn or expired link allows nothing any more: only say what was used.
    if (s.status !== "active") return h("td", { class: "nowrap muted" }, t("{used} of {max} used", { used: s.downloads_used, max: s.max_downloads }));
    const left = Math.max(0, s.max_downloads - s.downloads_used);
    return h("td", { class: "nowrap" },
      h("span", { class: left === 0 ? "soon" : null }, left === 0 ? "none left" : t("{n} left", { n: left })),
      h("div", { class: "sub" }, t("{used} of {max} used", { used: s.downloads_used, max: s.max_downloads })));
  }

  // Two kinds of links when upload links are switched on: to look at studies, and to send them in.
  const uploads = state.user.features.upload_links && state.user.features.upload_links.enabled;
  if (!uploads) state.sharesTab = "view";
  const showUploads = state.sharesTab === "upload";
  const tabs = uploads ? h("div", { class: "subtabs", role: "tablist", "aria-label": "Shares" },
    [["view", "Links to look at studies"], ["upload", "Upload links (they send us studies)"]].map(([id, label]) => h("button", {
      type: "button", role: "tab", "aria-selected": String(state.sharesTab === id || (id === "view" && !showUploads)),
      onclick: () => { state.sharesTab = id; renderApp(); },
    }, label))) : null;
  if (showUploads) {
    return h("div", { class: "view" }, h("div", { class: "page-head" }, h("h1", null, "Shares")), tabs, buildUploadLinksView());
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Shares"),
      h("div", { class: "head-actions" },
        actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load }),
        (state.user.features.share.patient || state.user.features.share.label) && can("read")
          ? h("button", { class: "btn primary", type: "button", onclick: () => openAdvancedShare(load) }, "New share...") : null)),
    tabs,
    h("p", { class: "muted" }, "Invitations for people outside: a link plus a password that opens one study. The password of an active link can be shown again (each time is recorded in the audit log); revoke a link if it was sent to the wrong person."),
    container);
}

/**
 * Advanced sharing (Shares > New share...): every study of a patient, or every study with a label, in
 * one link. Step 1 chooses and previews exactly which studies; step 2 is the Share window.
 */
function openAdvancedShare(reload) {
  const feature = state.user.features.share;
  const kinds = [
    feature.patient ? ["patient", "Every study of a patient"] : null,
    feature.label ? ["label", "Every study with a label"] : null,
  ].filter(Boolean);
  let kind = kinds[0][0];
  let found = [];
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const preview = h("div", { class: "share-preview" });
  const next = h("button", { class: "btn primary", type: "submit", disabled: true }, "Continue");
  const patientId = h("input", { name: "patient_id", placeholder: "e.g. P-001 (as shown in Search)", autocomplete: "off" });
  const labelSelect = h("select", { name: "label" }, h("option", { value: "" }, "Choose a label"));
  const patientField = h("label", null, "Patient ID", h("div", { class: "inline-field" }, patientId,
    h("button", { class: "btn", type: "button", onclick: () => look() }, "Find")));
  const labelField = h("label", null, "Label", labelSelect);
  if (feature.label) {
    loadLabels().then((labels) => labelSelect.append(...labels.filter((l) => l.count !== 0).map((l) =>
      h("option", { value: l.name }, l.count != null ? `${l.name} (${l.count})` : l.name)))).catch(() => {});
  }
  labelSelect.addEventListener("change", () => look());
  patientId.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); look(); } });

  /** Shows which studies would be shared (the same search as the Studies page). */
  async function look() {
    error.hidden = true;
    next.disabled = true;
    found = [];
    const params = new URLSearchParams({ limit: 51 });
    if (kind === "patient") {
      if (!patientId.value.trim()) { preview.replaceChildren(); return; }
      params.set("patient_id", `"${patientId.value.trim()}"`); // exactly this ID
    } else {
      if (!labelSelect.value) { preview.replaceChildren(); return; }
      params.set("labels", labelSelect.value);
      params.set("labels_mode", "all");
    }
    preview.replaceChildren(h("div", { class: "loading" }, "Loading..."));
    try {
      const page = await api("GET", `/api/studies?${params}`);
      found = page.items;
      if (!found.length) { preview.replaceChildren(h("p", { class: "muted" }, "No study found.")); return; }
      if (found.length > 50 || page.has_more) {
        preview.replaceChildren(h("p", { class: "form-error" }, "More than 50 studies: one link shares at most 50."));
        return;
      }
      // The same ID under two names: say so, the person sharing decides.
      const mixed = kind === "patient" && new Set(found.map((s) => s.patient_name)).size > 1
        ? h("p", { class: "form-error" }, "These studies have different patient names: check that the ID is the right one.") : null;
      preview.replaceChildren(mixed, h("p", null, h("b", null, t("{n} studies will be shared:", { n: found.length }))),
        h("ul", null, found.slice(0, 10).map((s) => h("li", null, studyLine(s))), found.length > 10 ? h("li", null, t("and {n} more", { n: found.length - 10 })) : null));
      next.disabled = false;
    } catch (e) {
      preview.replaceChildren();
      error.textContent = e.message;
      error.hidden = false;
    }
  }

  const choice = kinds.length > 1 ? h("div", { class: "subtabs", role: "tablist", "aria-label": "What to share" },
    kinds.map(([id, label]) => h("button", {
      type: "button", role: "tab", "aria-selected": String(id === kind), "data-kind": id,
      onclick: (e) => {
        kind = id;
        choice.querySelectorAll("[role=tab]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.kind === id)));
        patientField.hidden = kind !== "patient";
        labelField.hidden = kind !== "label";
        e.currentTarget.blur();
        look();
      },
    }, label))) : h("p", null, h("b", null, kinds[0][1]));
  patientField.hidden = kind !== "patient";
  labelField.hidden = kind !== "label";

  const dialog = h("dialog", { class: "share-dialog advanced-share" }, h("form", {
    class: "dialog-form",
    onsubmit: (event) => {
      event.preventDefault();
      if (!found.length) return;
      const how = kind === "label" ? { label: labelSelect.value } : { patient: true };
      dialog.close();
      // The list is refreshed when the Share window closes (the new link is in it).
      openShareDialog(found, how).addEventListener("close", () => reload(), { once: true });
    },
  },
    h("h2", null, "New share"),
    h("p", { class: "muted" }, "One link and one password for several studies: every study of a patient, or every study with a label. You see exactly which ones before creating the link."),
    choice, patientField, labelField, error, preview,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), next)));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  (kind === "patient" ? patientId : labelSelect).focus();
}

/* ---- Upload links: people outside send studies in ---- */

/** Bytes as "3.2 MB" / "1.4 GB". */
const megabytes = (bytes) => formatBytes(bytes || 0);

/** Shares > Upload links: the list, and "New upload link". */
function buildUploadLinksView() {
  const container = h("section", { class: "panel" });
  async function load() {
    container.replaceChildren(h("div", { class: "loading" }, "Loading..."));
    try {
      const links = await api("GET", "/api/upload-links");
      container.replaceChildren(links.length ? renderTable(links) : h("div", { class: "empty" }, "No upload link yet. Create one to ask somebody outside to send you studies (a CD of earlier images, a referral...)."));
    } catch (e) {
      container.replaceChildren(h("div", { class: "empty" }, e.message));
      toastError(e);
    }
  }
  function renderTable(links) {
    const now = Date.now() / 1000;
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Note", "By", "Created", "Time left", "Received", "Status", "Actions"].map((x) => h("th", { class: x === "Actions" ? "num" : null }, x)))),
      h("tbody", null, links.map((l) => h("tr", null,
        h("td", null, l.note || dash(), l.label ? h("div", { class: "sub" }, t("label: {label}", { label: l.label })) : null, l.notify ? h("div", null, h("span", { class: "chip" }, "emails me")) : null),
        h("td", null, l.created_by),
        h("td", { class: "nowrap" }, formatTimestamp(l.created_at)),
        l.status === "active"
          ? h("td", { class: "nowrap" }, formatDuration(l.expires_at - now), h("div", { class: "sub" }, t("until {date}", { date: formatTimestamp(l.expires_at) })))
          : h("td", { class: "nowrap muted" }, l.status === "revoked" ? "withdrawn" : "ended"),
        h("td", { class: "nowrap" },
          t("{n} files, {studies} studies", { n: l.files_received, studies: l.study_count }),
          h("div", { class: "sub" }, t("{used} of {max}", { used: megabytes(l.bytes_received), max: megabytes(l.max_bytes) }),
            l.files_rejected ? ` · ${t("{n} not DICOM", { n: l.files_rejected })}` : "")),
        h("td", null, h("span", { class: `chip ${l.status === "active" ? "status-on" : "status-off"}` }, l.status)),
        h("td", null, h("div", { class: "actions" },
          l.study_count ? actionButton("button", "search", "Studies", "The studies received", { class: "btn small", type: "button", onclick: () => openUploadLinkStudies(l) }) : null,
          l.url && l.status === "active" ? copyButton("Copy link", () => l.url, "link") : null,
          l.url && l.status === "active" && state.user.features.share.qr ? actionButton("button", "qr", "QR code", "The QR code of the link (to print)", { class: "btn small", type: "button", onclick: () => openQrDialog(`/api/upload-links/${l.id}/qr`, t("QR code of the upload link")) }) : null,
          l.password_available ? actionButton("button", "key", "Password", "Show the password", {
            class: "btn small", type: "button",
            onclick: async () => {
              try { const r = await api("POST", `/api/upload-links/${l.id}/password`); passwordDialog({ label: l.note || t("Upload link"), note: "" }, "Password of the link", r.password, "Give it by a different channel than the link. Showing it was recorded in the audit log."); } catch (e) { toastError(e); }
            },
          }) : null,
          l.status !== "revoked" ? actionButton("button", "edit", "Edit...", "More time or a bigger size limit", { class: "btn small", type: "button", onclick: () => openUploadLinkEditor(l, load) }) : null,
          l.status === "active" ? actionButton("button", "revoke", "Revoke", "Revoke this link", {
            class: "btn small danger", type: "button",
            onclick: async () => {
              if (!confirm(t("Revoke this link? The person will not be able to send anything more."))) return;
              try { await api("POST", `/api/upload-links/${l.id}/revoke`); toast("Link revoked."); load(); } catch (e) { toastError(e); }
            },
          }) : null)))))));
  }
  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "Upload links"),
      h("div", { class: "head-actions" },
        actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load }),
        h("button", { class: "btn primary", type: "button", onclick: () => openUploadLinkDialog(load) }, "New upload link"))),
    h("p", { class: "muted" }, "A link plus a password that lets somebody outside SEND you studies: they drop the files or a whole CD folder on a page, and the studies arrive in the archive. They see nothing else of it."),
    container);
}

/** Creating an upload link: how long, how much, a label, a note; then the link, the password and the QR code. */
function openUploadLinkDialog(reload) {
  const feature = state.user.features.upload_links;
  const share = state.user.features.share;
  const dialog = h("dialog", { class: "share-dialog" });
  dialog.addEventListener("close", () => { dialog.remove(); reload(); });
  document.body.append(dialog);
  if (!share.configured) {
    dialog.replaceChildren(h("div", { class: "dialog-form" }, h("h2", null, "Sharing is not set up yet"),
      h("p", null, "An administrator must first enter the public address of the share links in Settings, section Share links."),
      h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
    dialog.showModal();
    return;
  }
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const duration = buildDurationInput(feature.default_hours * 60, feature.max_hours, "The link works for");
  const size = h("input", { type: "number", min: 1, max: feature.max_mb, value: Math.min(feature.max_mb, 2048), required: true });
  const label = h("input", { value: feature.label || "", maxlength: 64, placeholder: "received", autocomplete: "off" });
  const note = h("input", { maxlength: 120, placeholder: "For your own records, e.g. Mrs Doe, previous MRI from St Mary's", autocomplete: "off" });
  const notify = h("input", { type: "checkbox", disabled: !state.user.email || !feature.notify });
  const emailOptions = buildEmailOptions(() => true, true);
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
        const r = await api("POST", "/api/upload-links", {
          minutes: duration.minutes, max_mb: Number(size.value), label: label.value, note: note.value,
          notify: notify.checked, email: emailOptions.value,
        });
        showResult(r);
      } catch (e) {
        error.textContent = e.message;
        error.hidden = false;
        submit.disabled = false;
      }
    },
  },
    h("h2", null, "Ask somebody outside to send studies"),
    error,
    duration.node,
    h("label", null, t("Most they can send, in MB (up to {max})", { max: feature.max_mb }), size),
    h("label", null, "Label put on the studies received (empty = none)", label),
    h("label", null, "Note (optional)", note),
    feature.notify ? h("div", null, h("label", { class: "check" }, notify, "Email me when files arrive"),
      state.user.email ? null : h("p", { class: "hint" }, "Add your email address in My account first.")) : null,
    emailOptions.node,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), submit)));

  function showResult(r) {
    const expires = new Date(r.expires_at * 1000).toLocaleString(i18n.locale);
    dialog.replaceChildren(h("div", { class: "dialog-form" },
      h("h2", null, "Upload link created"),
      h("label", null, "Link", h("div", { class: "secret-box" }, h("input", { readonly: true, value: r.url, onfocus: (e) => e.target.select() }), copyButton("Copy link", () => r.url))),
      h("div", null, h("div", { class: "muted" }, "Password"),
        h("div", { class: "secret-box" }, h("span", { class: "password" }, r.password), copyButton("Copy password", () => r.password))),
      h("p", { class: "muted" }, t("Valid until {expires}, for up to {mb} MB. Give the password by a different channel than the link.", { expires, mb: r.max_mb })),
      share.qr ? qrBlock(`/api/upload-links/${r.id}/qr`, "upload-link-qr-code.svg") : null,
      emailOutcome(r.email),
      h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  }
  dialog.showModal();
}

/** More time (from now) or a bigger limit for an upload link. */
function openUploadLinkEditor(link, reload) {
  const feature = state.user.features.upload_links;
  const now = Date.now() / 1000;
  const active = link.status === "active";
  const initialMinutes = active ? Math.max(1, Math.ceil((link.expires_at - now) / 60)) : feature.default_hours * 60;
  const duration = buildDurationInput(Math.min(initialMinutes, feature.max_hours * 60), feature.max_hours, "The link works for, from now");
  const size = h("input", { type: "number", min: 1, max: feature.max_mb, value: Math.ceil(link.max_bytes / 1048576), required: true });
  formDialog("Change the upload link", link.note || null, [duration.node, h("label", null, t("Most they can send, in MB (up to {max})", { max: feature.max_mb }), size)], "Save", async () => {
    const problem = duration.problem();
    if (problem) throw new Error(problem);
    const body = { max_mb: Number(size.value) };
    if (!active || duration.minutes !== initialMinutes) body.minutes = duration.minutes;
    await api("PUT", `/api/upload-links/${link.id}`, body);
    toast("Link updated.");
    reload();
  });
}

/** The studies received through an upload link. */
async function openUploadLinkStudies(link) {
  let studies;
  try { studies = await api("GET", `/api/upload-links/${link.id}/studies`); } catch (e) { toastError(e); return; }
  const dialog = h("dialog", { class: "wide-dialog" }, h("div", { class: "dialog-form" },
    h("h2", null, t("Received through the link: {note}", { note: link.note || `#${link.id}` })),
    studies.length ? renderStudiesTable(studies) : h("p", { class: "muted" }, "These studies are no longer in the archive."),
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
}

/* ====================================================================
   9c. HELP
   Instructions for everyday users, written for people who are not administrators. The page is
   built for the person reading it: only the parts their permissions give access to, and only the
   features switched on in this installation (Weasis, OHIF, email...). Buttons are shown in the
   text as the icons they have on screen.
   ==================================================================== */

/** A small inline copy of an icon, to show in the text which button is meant. */
const helpIcon = (name) => h("span", { class: "help-icon", "aria-hidden": "true" }, icon(name));
/** A button as it appears on screen: its icon, then its name. */
const helpButton = (iconName, text) => [helpIcon(iconName), " (", text, ")"];
/** A bullet list: one bullet per argument (an argument may be several pieces: text, icons...); null = left out. */
const ul = (...items) => h("ul", null, items.filter(Boolean).map((item) => h("li", null, item)));

/**
 * The sections of the Help page. `show` says whether this user sees it; `body` builds its content.
 * Keep the wording plain: the readers are clinical and front-desk staff, not IT people.
 */
function helpSections() {
  const f = state.user.features;
  const share = f.share || {};
  return [
    {
      id: "around", title: "Getting around", show: true,
      body: () => [
        h("p", null, "The column of icons on the left takes you to each part of the portal (on a phone, the icons are along the bottom of the screen). Rest the mouse on an icon to see its name. Buttons that show only an icon work the same way: rest the mouse on one to see what it does."),
        h("p", null, "What you can use:"),
        ul(
          [helpIcon("home"), " ", h("b", null, "Home"), ": the latest studies and an area to upload files."],
          can("read") ? [helpIcon("search"), " ", h("b", null, "Search"), ": find a study or a series."] : null,
          can("upload") ? [helpIcon("upload"), " ", h("b", null, "Uploads"), ": add images to the archive and follow the uploads."] : null,
          can("send") ? [helpIcon("transfers"), " ", h("b", null, "Transfers"), ": follow the studies you sent to another system."] : null,
          can("share") ? [helpIcon("share"), " ", h("b", null, "Shares"), ": the links you created to show a study to someone outside."] : null,
          opts().retrieve ? [helpIcon("retrieve"), " ", h("b", null, "Retrieve"), ": find studies on another system and bring them here."] : null,
          opts().worklists ? [helpIcon("worklist"), " ", h("b", null, "Worklists"), ": the examinations the scanners have to do."] : null,
          opts().account_self_service !== false || (f.two_factor || "off") !== "off"
            ? [helpIcon("account"), " ", h("b", null, "My account"), opts().account_self_service !== false ? ": your password, your email address" : ": two-step sign-in", opts().account_self_service !== false && f.two_factor && f.two_factor !== "off" ? ", two-step sign-in" : "", "."] : null,
          [helpIcon("help"), " ", h("b", null, "Help"), ": this page."],
        ),
        h("p", { class: "muted" }, "You only see the parts your account gives access to. If you need something that is not here, ask your administrator."),
      ],
    },
    {
      id: "home", title: "The home page", show: true,
      body: () => [
        h("p", null, t("It lists the {n} most recent studies, {order} first.", {
          n: f.simplified && f.simplified.recent_count ? f.simplified.recent_count : "",
          order: t(f.simplified && f.simplified.recent_order === "study_date" ? "the newest examination dates" : "the last ones received by the archive"),
        }), " ",
          helpButton("refresh", "Refresh"), " updates the list.",
          f.simplified && f.simplified.recent_auto_refresh ? " " + t("It also refreshes itself every {n} seconds; the \"Auto-refresh\" button next to it counts down the seconds to the next update; click it to pause (or start again). It waits while a window is open or the series of a study are shown.", { n: f.simplified.refresh_seconds }) : ""),
        can("read") ? h("p", null, "Each study has the same buttons as on the Search page (see below). The small arrow ▸ at the start of a row shows the series of the study.") : null,
        can("upload") ? h("p", null, "At the bottom, drag files or a whole folder onto the upload area to add them to the archive (see Uploading images).") : null,
      ],
    },
    {
      id: "search", title: "Finding a study", show: can("read"),
      body: () => [
        searchOptions().live
          ? h("p", null, "Fill in one or more boxes: the results follow as you type (dates, lists and buttons search at once). Leave a box empty to ignore it.")
          : h("p", null, "Fill in one or more boxes, then press ", h("b", null, "Search studies"), ". Leave a box empty to ignore it."),
        ul(
          [h("b", null, "Patient name"), ": part of the name is enough (DOE finds DOE, JOHN). A star works as a wildcard: SMI* finds every name starting with SMI."],
          [h("b", null, "Patient ID"), " and ", h("b", null, "Study description"), ": also a part is enough. Put a value between double quotes to find exactly it: \"P-001\" does not find P-0011."],
          [h("b", null, "Modality"), ": click the buttons (CT, MR, US...) to show only those examinations; you can choose several. Click again to remove one. Other codes can be typed in the box next to them."],
          [h("b", null, "Study date from / to"), ": a period; fill in only one of the two for \"since\" or \"until\"."],
        ),
        h("p", null, "The results show the newest studies first, 25 at a time (",
          searchOptions().infinite ? "the next ones appear when you scroll to the end of the list" : [h("b", null, "Previous"), " / ", h("b", null, "Next"), " at the bottom"], "). ",
          "Click a column title to sort by it; click it again for the other way round. ", h("b", null, "Clear filters"), " empties every box."),
        ul(
          "Click the small arrow ▸ at the start of a row to see the series of the study (each set of images) and how many images each has.",
          f.thumbnails && can("visualize") ? "The small picture is a preview of the study or series: it appears after a moment." : null,
          "Rest the mouse on a modality tag (CT, MR...) to see its full name.",
          [helpButton("patient", "Other studies of this patient"), ": every study of the same patient, in the Search page."],
          [helpButton("link", "Copy link"), " (next to the title): a link to this search with its filters, to bookmark or to send to a colleague."],
          labelsOn() ? [h("b", null, "Labels"), ": click labels to see only the studies that have them (several: any or all of them), or ", h("b", null, "Without labels"), "."] : null,
          [h("b", null, "Series"), " (the second tab of the Search page): the same search, one row per series, with a ", h("b", null, "Body part"), " box (CHEST, KNEE...)."],
        ),
      ],
    },
    {
      id: "labels", title: "Labels", show: can("read") && labelsOn() && canEditLabels(),
      body: () => [
        h("p", null, "Labels are short words you put on studies to find them again: urgent, insurance, second-opinion, teaching... Everyone sees them, in the Labels column and in the study window, and can search by them."),
        ul(
          [helpButton("tag", "Labels"), " on a study: type a label (or choose a suggested one) and press ", h("b", null, "Add"), "; the × of a label takes it off."],
          (searchOptions().allowed_labels || []).length ? t("Only these labels can be used: {labels}.", { labels: searchOptions().allowed_labels.join(", ") }) : "Letters, digits, - and _ only (no spaces).",
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
          viewerLinks([{ id: "x", study_instance_uid: "1" }]).some(([t]) => t !== "OHIF")
            ? [helpButton("viewers", "More viewers"), ": the other viewers of this portal (", viewerLinks([{ id: "x", study_instance_uid: "1" }]).map(([t]) => t).filter((t) => t !== "OHIF").join(", "), "), each in a new tab."] : null,
        ),
      ],
    },
    {
      id: "several", title: "Working on several studies at once", show: can("read") && bulkOn(),
      body: () => [
        h("p", null, "Tick the box at the start of each study (or the box in the title row for the whole page). A bar appears above the list with what you can do with them:"),
        ul(
          can("visualize") ? [h("b", null, "Open together"), ": the studies side by side in the viewer, to compare them."] : null,
          can("download") ? [h("b", null, "Download (one ZIP)"), ": every ticked study in one file."] : null,
          can("send") ? [h("b", null, "Send..."), ": to another system, one transfer per study."] : null,
          can("share") && opts().multi_share ? [h("b", null, "Share (one link)..."), ": one link and one password for all of them."] : null,
          canEditLabels() ? [h("b", null, "Labels..."), ": put a label on all of them, or take it off."] : null,
          opts().delete ? [h("b", null, "Delete..."), ": removes them for good, after a confirmation."] : null,
        ),
      ],
    },
    {
      id: "images", title: "Images, DICOM tags and reports", show: can("read") && !!opts().instances,
      body: () => [
        h("p", null, helpButton("images", "Images"), " on a series (open a study's series with ▸) lists its images. Choose one to see it, its ", h("b", null, "DICOM tags"),
          " (every piece of information stored in the file, with a filter box)", can("download") && opts().single_file ? ", or to download that one file" : "", "."),
        can("visualize") ? h("p", null, helpButton("pdf", "PDF report"), " on a study that contains a report (a DOC series) opens the PDF in a new tab.") : null,
      ],
    },
    {
      id: "manage", title: "Changing or deleting a study", show: !!(opts().modify || opts().anonymize || opts().add_series || opts().delete),
      body: () => [
        h("p", null, helpButton("more", "Manage"), " on a study (and on a series) offers:"),
        ul(
          opts().modify ? [h("b", null, "Change details"), ": correct the patient name, ID, birth date, description... The images are rewritten; tick \"Keep the DICOM identifiers\" if the study must stay the same study for other systems."] : null,
          opts().modify ? [h("b", null, "Move"), ": a study to another patient (for a study filed under the wrong patient), a series to another study or to a new one."] : null,
          opts().anonymize ? [h("b", null, "Anonymize"), ": a copy without the patient's identity (for teaching, research...). Give it a name and ID, or let the portal make them up."] : null,
          opts().add_series ? [h("b", null, "Add a series"), ": PDF files (reports) or pictures (PNG, JPEG) become a new series of the study."] : null,
          opts().delete ? [h("b", null, "Delete"), ": removes the study, series or image for good. There is no undo."] : null,
        ),
        h("p", { class: "muted" }, "These changes run in the background: a message says when they are done, and the lists update. Every change is recorded in the audit log."),
      ],
    },
    {
      id: "retrieve", title: "Bringing in studies from another system", show: !!opts().retrieve,
      body: () => [
        h("p", null, "On the ", helpIcon("retrieve"), " ", h("b", null, "Retrieve"), " page, choose the other system (another archive, a scanner...), fill in a name, an ID, an accession number or dates, and press ", h("b", null, "Search there"), "."),
        h("p", null, h("b", null, "Retrieve"), " brings a study into this archive; follow it on the Transfers page, which then offers ", h("b", null, "Open the study"), ". Studies that are already here say so and can be opened at once."),
      ],
    },
    {
      id: "worklists", title: "Worklists", show: !!opts().worklists,
      body: () => [
        h("p", null, "The ", helpIcon("worklist"), " ", h("b", null, "Worklists"), " page lists the examinations the scanners see when they ask for their worklist: patient, modality, date and time, description. ",
          h("b", null, "Add a worklist"), " creates one; the buttons of a row change or delete it."),
        h("p", { class: "muted" }, "Type the patient name as DOE^JANE (family name, ^, given name), like the scanners expect."),
      ],
    },
    {
      id: "download", title: "Downloading a study", show: can("download"),
      body: () => [
        h("p", null, helpButton("download", "Download"), " (on a study, or on one series) offers two kinds of ZIP file:"),
        ul(
          [h("b", null, "ZIP"), ": the image files in folders. Choose this one in most cases."],
          [h("b", null, "ZIP with DICOMDIR"), ": the same with an index, like the CD or DVD of a scanner; some programs need it."],
          opts().cd_export ? [h("b", null, "Patient CD / USB"), ": a ZIP to unpack onto a disc or a USB stick for the patient: the images with their index, a page that lists the studies and, if your practice set one up, a viewer program. (Also for several ticked studies.)"] : null,
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
          "Follow it on the ", helpIcon("transfers"), " ", h("b", null, "Transfers"), " page",
          ": while it runs it shows the percentage, how many images were sent, how long it has been running and about how long is left (the list updates itself); then how many images were sent, or why it failed."),
        h("p", null, "Rest the mouse on the name of a destination (for example \"ris\") to see what it is and where it is."),
        f.simplified && f.simplified.transfers_auto_refresh ? h("p", null, t("The Transfers page also refreshes itself every {n} seconds, to show transfers started by colleagues; \"Auto-refresh\" next to Refresh pauses it.", { n: f.simplified.refresh_seconds })) : null,
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
            h("li", null, t("Choose how long the link works (in days, hours and minutes, at most {duration}) and how many times the study may be downloaded (0 = online viewing only). Add a note for yourself if you like (for example \"Dr. Smith, second opinion\"): the person does not see it.", { duration: durationWords((share.max_hours || 0) * 60) })),
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
            share.qr ? [helpButton("qr", "QR code"), ": the link as a QR code, to print on a handout (the phone's camera opens it; it does not contain the password)."] : null,
          ),
          share.patient || share.label ? h("p", null, h("b", null, "New share..."), " on the Shares page puts ",
            [share.patient ? "every study of a patient" : null, share.label ? "every study with a label" : null].filter(Boolean).join(" or "),
            " in one link: type the patient ID or choose the label, check the list of studies, then continue as for one study.") : null,
          share.anonymize || share.notify ? h("p", null, h("b", null, "More options"), " in the Share window: ",
            [share.anonymize ? "share an anonymized copy (without the patient's name and ID; deleted when the link ends)" : null,
              share.notify ? "email me when the link is opened or downloaded (needs your email address in My account)" : null].filter(Boolean).join("; "), ".") : null,
        ]),
    },
    {
      id: "uploadlinks", title: "Asking someone outside to send studies", show: !!(f.upload_links && f.upload_links.enabled),
      body: () => [
        h("p", null, "On the Shares page, ", h("b", null, "Upload links"), " > ", h("b", null, "New upload link"), " makes a link plus a password for a person outside (another clinic, a patient with a CD of earlier images). They open the link, type the password and drag the files or the whole CD folder onto the page. The studies arrive in the archive with the label you chose; they see nothing else."),
        ul("Give the password by a different way than the link, as for share links.",
          "The list shows how many files and studies arrived; the search button lists the studies received.",
          "Files that are not DICOM images (a viewer program on the CD, a ZIP archive...) are skipped and counted apart. Files of a study already in the archive are refused: an outsider can never add images to an existing study."),
      ],
    },
    {
      id: "account", title: "My account", show: opts().account_self_service !== false || (f.two_factor || "off") !== "off",
      body: () => [
        opts().account_self_service !== false ? h("p", null, helpIcon("account"), " ", h("b", null, "My account"), " lets you change your password and your email address.") : null,
        f.two_factor && f.two_factor !== "off" ? h("p", null, h("b", null, "Two-step sign-in"), ": after the password, the portal asks for a 6-digit code from an app on your phone. ",
          f.two_factor === "required" ? "It is required in this portal: you set it up at your first sign-in." : "Switch it on in My account: scan the QR code with the app and type the code it shows.",
          " Keep the recovery codes somewhere safe: each one signs you in once if you lose your phone. Lost them too? Ask your administrator to reset it.") : null,
        state.user.features.recently_viewed > 0 ? h("p", null, h("b", null, "Recently viewed"), " on the home page lists the studies you opened last. Only you see it; \"Clear the list\" empties it.") : null,
      ],
    },
    {
      id: "upload", title: "Uploading images", show: can("upload"),
      body: () => [
        h("p", null, "To add images to the archive (from a CD, a USB stick, another program...):"),
        ul(
          ["Drag the files, or a whole folder, onto the upload area of the Home page or of ", h("b", null, "Uploads"), "."],
          ["Or press ", h("b", null, "Choose files"), " / ", h("b", null, "Choose folder"), " to pick them on your computer."],
        ),
        h("p", null, "DICOM image files and ZIP archives are accepted. Images that are already in the archive are not copied twice (they are counted as \"already stored\")."),
        h("p", null, "The ", h("b", null, "Uploads"), " page shows each upload with its progress, speed and time left, then how many images were new, already stored or failed. ",
          "While images are being sent, the Uploads icon shows the percentage, so you can do something else in the meantime."),
        h("p", { class: "muted" }, "Do not close or reload the page while an upload is running: it would stop. When it is finished, the new studies appear in the search and on the home page."),
      ],
    },
    {
      id: "session", title: "Signing out and good habits", show: true,
      body: () => [
        ul(
          [helpButton("logout", "Log out"), " when you leave a shared computer: the portal shows patient data."],
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
    h("p", { class: "muted" }, t("How to use {portal}. This page describes what your account can do.", { portal: state.brand.name })),
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
      h("span", null, `Orthanc: ${translateMessage(state.orthancProblem)} `),
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
    { key: "public_base_url", label: "Public address of the portal (optional)", placeholder: "https://pacs.example.com", hint: "The address staff use to open the portal: given to Weasis, and used for the links in the auto-forward emails. Empty = automatic for Weasis (the address you used to open the portal) and no link in the emails. (Weasis is for staff on the intranet; guests only get the browser viewer.)" },
  ],
  email: [
    { key: "smtp_host", label: "Mail server", placeholder: "smtp.example.com", hint: "Your practice's outgoing mail server (SMTP). Empty = the portal sends no email." },
    { key: "smtp_port", label: "Port", type: "number", min: 1, max: 65535, hint: "Usually 587 (STARTTLS), 465 (TLS) or 25 (a relay on your network)." },
    { key: "smtp_security", label: "Encryption", type: "select", options: [["starttls", "STARTTLS (usually port 587)"], ["tls", "TLS (usually port 465)"], ["none", "None (only for a relay on your own network)"]] },
    { key: "smtp_auth", label: "Sign-in to the mail server", type: "select", wide: true,
      options: [["password", "User name and password (or none, for a relay)"], ["microsoft365", "Microsoft 365 / Exchange Online (OAuth 2.0)"]],
      hint: "Microsoft 365 no longer accepts a password for sending mail: choose Microsoft 365 and register an app in Microsoft Entra ID (README, section on email)." },
    { key: "smtp_username", label: "User name on the mail server", hint: "Empty when the server accepts the portal without signing in (a relay on your network)." },
    { key: "smtp_oauth_tenant", label: "Directory (tenant) ID", placeholder: "00000000-0000-0000-0000-000000000000", m365: true,
      hint: "Entra ID > Overview. The tenant's domain also works, e.g. contoso.onmicrosoft.com." },
    { key: "smtp_oauth_client_id", label: "Application (client) ID", placeholder: "00000000-0000-0000-0000-000000000000", m365: true,
      hint: "Entra ID > App registrations > your app > Overview." },
    { key: "smtp_oauth_token_url", label: "Token address (advanced)", placeholder: "Empty = Microsoft's (login.microsoftonline.com)", m365: true,
      hint: "Only for a national cloud, e.g. https://login.microsoftonline.us/<tenant>/oauth2/v2.0/token." },
    { key: "smtp_oauth_scope", label: "Scope (advanced)", placeholder: "Empty = https://outlook.office365.com/.default", m365: true,
      hint: "Only for a national cloud, e.g. https://outlook.office365.us/.default." },
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
    { key: "share_anonymize", label: "Offer \"Share an anonymized copy\"", type: "checkbox", wide: true,
      hint: "Orthanc makes a copy without the patient's name and ID (labelled shared-anonymized) and the link shows that copy. The copy is deleted when the link is revoked, or 30 days after it expires." },
    { key: "share_patient", label: "Offer \"Every study of a patient, in one link\" (Shares page)", type: "checkbox", wide: true,
      hint: "Also needs \"One share link for several studies\" (Features)." },
    { key: "share_label", label: "Offer \"Every study with a label, in one link\" (Shares page)", type: "checkbox", wide: true,
      hint: "Also needs labels and \"One share link for several studies\" (Features). At most 50 studies per link." },
    { key: "share_qr", label: "QR codes of the links (share and upload links)", type: "checkbox", wide: true,
      hint: "In the Share window and the Shares list, to print on a handout. A QR code never contains the password." },
    { key: "share_notify", label: "Offer \"Email me when the link is opened or downloaded\"", type: "checkbox", wide: true,
      hint: "Also \"Email me when files arrive\" of upload links. Needs the mail server below and an email address on the person's account (My account, or Users). The emails name the link, never the patient. Switching it off also stops the notices already asked for." },
    { key: "share_email_default", label: "Tick \"Send the invitation by email\" by default", type: "checkbox", wide: true,
      hint: "In the windows that create a share or upload link. Staff then type the address, or untick it to give the link by hand. Needs the mail server below; editing an existing link never emails unless ticked." },
  ],
  security: [
    { key: "two_factor", label: "Two-step sign-in (a code from a phone app after the password)", type: "select", wide: true,
      options: [["off", "Off: the password is enough"], ["optional", "Optional: each person may switch it on in My account"], ["required", "Required for everybody: set up at the next sign-in"]],
      hint: "Any authenticator app works (Microsoft or Google Authenticator, FreeOTP...). Each person also gets ten one-time recovery codes for a lost phone; an administrator can reset it in Users. Switching it off stops asking for codes (what was set up is kept)." },
    { key: "account_self_service", label: "Let each person change their own password and email address (My account)", type: "checkbox", wide: true,
      hint: "Off: only an administrator can (Users). My account then only shows two-step sign-in, when it is used." },
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
  lists: [
    { key: "search_as_you_type", label: "Search while typing (live search)", type: "checkbox", wide: true,
      hint: "On: the results follow the boxes as they are filled in (after a short pause, once the number of letters below is typed); dates, lists and buttons search at once. Off: press the Search button." },
    { key: "search_min_chars", label: "Letters before searching while typing", type: "number", min: 1, max: 10 },
    { key: "infinite_scroll", label: "Endless lists", type: "checkbox", wide: true,
      hint: "On: the next results are loaded when you scroll to the end of the list (there is also a Load more button). Off: 25 results per page with Previous / Next." },
  ],
  labels: [
    { key: "available_labels", label: "Allowed labels", type: "list", wide: true, placeholder: "urgent, insurance, second-opinion, teaching",
      hint: "Separate them with commas. Empty = staff may create any label. Letters, digits, - and _ only. Labels already on studies can always be taken off." },
    { key: "labels_enabled", label: "Use labels", type: "checkbox", wide: true,
      hint: "Untick to hide labels everywhere: no Labels column, filter, button or Help section (the labels stay in Orthanc)." },
    { key: "label_counts", label: "Show how many studies have each label", type: "checkbox", wide: true,
      hint: "Orthanc counts them each time the labels are shown: switch it off if that is slow on a very large archive." },
  ],
  features: [
    { key: "bulk_actions", label: "Tick boxes on the studies lists, to act on several studies at once", type: "checkbox", wide: true,
      hint: "Download them in one ZIP, send them, share them with one link, put a label on them, open them together, delete them: what each person may do." },
    { key: "multi_study_shares", label: "One share link for several studies", type: "checkbox", wide: true },
    { key: "instance_browser", label: "Images, DICOM tags and PDF reports", type: "checkbox", wide: true,
      hint: "An Images button on each series: the list of images, a picture of each, all their DICOM tags; and a PDF button on studies that contain a report." },
    { key: "single_file_download", label: "Download one image (a single DICOM file)", type: "checkbox", wide: true, hint: "Needs the download permission." },
    { key: "nifti_export", label: "Download a series as NIfTI", type: "checkbox", wide: true, hint: "Needs Orthanc's neuro plugin." },
    { key: "download_name_template", label: "Name of downloaded ZIP files", maxlength: 120, placeholder: "{patient_id}_{study_date}_{id}",
      hint: "With {patient_id}, {patient_name}, {study_date}, {description}, {accession}, {modality} and {id} (the start of Orthanc's id)." },
    { key: "allow_delete", label: "Delete studies, series and images", type: "checkbox", wide: true,
      hint: "For people with the delete permission (and administrators). Every deletion is in the audit log. It cannot be undone." },
    { key: "allow_modify", label: "Change patient, study and series details; move studies and series", type: "checkbox", wide: true,
      hint: "For people with the edit permission. Moves a study to another patient, a series to another (or a new) study." },
    { key: "allow_anonymize", label: "Anonymize studies and series", type: "checkbox", wide: true, hint: "Edit permission. Makes an anonymized copy (or replaces the original)." },
    { key: "allow_add_series", label: "Add a series to a study (PDF files, pictures)", type: "checkbox", wide: true, hint: "Edit permission." },
    { key: "send_to_peers", label: "Send to the other Orthanc servers (Orthanc peers)", type: "checkbox", wide: true, hint: "As configured in Orthanc (OrthancPeers)." },
    { key: "send_to_dicomweb", label: "Send to DICOMweb servers", type: "checkbox", wide: true, hint: "As configured in Orthanc's DICOMweb plugin (DicomWeb.Servers)." },
    { key: "query_retrieve", label: "Retrieve: search other systems and bring studies in", type: "checkbox", wide: true,
      hint: "A Retrieve page for people with the retrieve permission: search a DICOM node (C-FIND / C-MOVE) or a DICOMweb server, then bring the study here." },
    { key: "worklists_enabled", label: "Worklists (what the scanners have to do)", type: "checkbox", wide: true,
      hint: "A Worklists page for people with the worklists permission. Needs Orthanc 1.12.10+ with the worklists plugin and \"SaveInOrthancDatabase\": true." },
    { key: "jobs_page", label: "Configuration > Jobs: every Orthanc job", type: "checkbox", wide: true },
    { key: "system_page", label: "Configuration > System: Orthanc's version, storage, plugins, log level, metrics", type: "checkbox", wide: true },
    { key: "raw_api", label: "System: read Orthanc's REST API (GET only)", type: "checkbox", wide: true, hint: "For troubleshooting. Never files; each read is in the audit log." },
    { key: "stats_page", label: "Configuration > Statistics: studies per month and modality, links, transfers, who uploads", type: "checkbox", wide: true },
    { key: "cd_export", label: "Patient CD / USB in the download menus", type: "checkbox", wide: true,
      hint: "A ZIP to copy onto a disc or a USB stick: the images with their DICOMDIR index, a page for the patient and, if you give its folder below, a portable viewer. Needs the download permission." },
    { key: "cd_viewer_dir", label: "Folder of a portable DICOM viewer for the patient CDs (optional)", wide: true, placeholder: "/data/cd-viewer   or   D:\\CD-Viewer",
      hint: "A full path on the computer (or in the container) running the portal, e.g. an unpacked portable Weasis with its autorun.inf. Its files are copied onto every patient CD. In Docker, mount the folder (see the README)." },
    { key: "upload_links", label: "Upload links: people outside send studies in", type: "checkbox", wide: true,
      hint: "Staff with the share AND upload permissions can create a link plus a password; the person drops files or a CD folder on a page of the internet side and the studies arrive here. They see nothing of the archive." },
    { key: "upload_link_default_hours", label: "Upload link validity, default (hours)", type: "number", min: 1, max: 8760, hint: "Never more than the maximum of share links (Network & sharing)." },
    { key: "upload_link_max_mb", label: "Most an upload link can receive (MB)", type: "number", min: 1, max: 1048576 },
    { key: "upload_link_label", label: "Label put on received studies (default)", maxlength: 64, placeholder: "received", hint: "Empty = none. Each link can choose another one." },
  ],
  forward: [
    { key: "forward_enabled", label: "Apply the rules below", type: "checkbox", wide: true,
      hint: "Every study that arrives and matches a rule is sent on (or announced) as soon as Orthanc considers it complete (no new image for a while: Orthanc's StableAge). Switching it on starts from now: studies that arrived before are not sent." },
  ],
  backups: [
    { key: "backups_enabled", label: "Backups (automatic ones, Back up now, Download a copy)", type: "checkbox", wide: true,
      hint: "Off: no backup is made or downloaded from the portal (back up the database file yourself)." },
    { key: "backup_dir", label: "Backup folder", wide: true, placeholder: "/data/backups   or   D:\\Backups\\Portal",
      hint: "A full path on the computer (or in the container) running the portal. Empty = no automatic backups. Better on another disk, or in a folder that is itself backed up." },
    { key: "backup_interval_hours", label: "Hours between two backups", type: "number", min: 1, max: 720 },
    { key: "backup_keep", label: "Backups kept", type: "number", min: 1, max: 365, hint: "The oldest are deleted." },
  ],
  moreviewers: [
    { key: "stone_viewer", label: "Stone Web Viewer", type: "select", options: [["auto", "Automatic (when Orthanc has the plugin)"], ["on", "Always"], ["off", "Never"]] },
    { key: "volview_viewer", label: "VolView (3D)", type: "select", options: [["auto", "Automatic (when Orthanc has the plugin)"], ["on", "Always"], ["off", "Never"]], hint: "Opens a downloaded copy of the study: needs the download permission." },
    { key: "wsi_viewer", label: "Whole-slide viewer (microscopy, SM series)", type: "select", options: [["auto", "Automatic (when Orthanc has the plugin)"], ["on", "Always"], ["off", "Never"]] },
    { key: "meddream_url", label: "MedDream address (optional)", placeholder: "https://meddream.example.com/?study={study_uid}", wide: true,
      hint: "A MedDream viewer of your own: its address with {study_uid} where the study goes. Empty = no MedDream button." },
  ],
  formatting: [
    { key: "date_format", label: "Dates", type: "select", options: [["iso", "2024-05-17"], ["dmy", "17/05/2024"], ["mdy", "05/17/2024"], ["dmy_dots", "17.05.2024"], ["long", "17 May 2024"]] },
    { key: "name_format", label: "Patient names", type: "select", options: [["last_first", "DOE, JOHN"], ["first_last", "JOHN DOE"], ["dicom", "DOE^JOHN (as stored)"]] },
    { key: "language", label: "Language", type: "select", options: [["en", "English"], ["es", "Español"]],
      hint: "The language of the portal for everyone, of the invitation page and of the invitation emails. Each person can still choose another language for their own browser (the menu at the top of the page)." },
    { key: "custom_css", label: "Custom CSS (advanced)", type: "textarea", wide: true, maxlength: 50000, rows: 6, placeholder: ".simple-head { border-bottom: 3px solid #c00; }",
      hint: "Extra style rules for the portal and the invitation page. No @import or url(...): everything stays on this server." },
  ],
  simplified: [
    { key: "simplified_recent_count", label: "Recent studies on the home page", type: "number", min: 1, max: 100 },
    { key: "simplified_recent_order", label: "Which studies are the most recent", type: "select", options: [["received", "The last ones received by the PACS"], ["study_date", "The newest study dates"]] },
    { key: "simplified_recent_layout", label: "How the recent studies are shown", type: "select", options: [["table", "Table, like the Studies page (same columns and buttons)"], ["cards", "Cards with big previews"]] },
    { key: "simplified_card_size", label: "Size of the study cards", type: "select", options: [["small", "Small (more per row)"], ["medium", "Medium"], ["large", "Large (bigger previews)"]], hint: "Only for the cards." },
    { key: "simplified_recent_title", label: "Title of the recent studies", placeholder: "Recent studies", maxlength: 60 },
    { key: "simplified_upload_text", label: "Text of the upload area", placeholder: "Drag a file to upload it", maxlength: 120 },
    { key: "simplified_show_total", label: "Show the total number of studies in the header", type: "checkbox", wide: true },
    { key: "simplified_show_labels", label: "Write the name under each icon", type: "checkbox", wide: true, hint: "Otherwise the name pops up when the mouse rests on an icon." },
    { key: "auto_refresh_seconds", label: "Auto-refresh every (seconds)", type: "number", min: 5, max: 3600, hint: "30 seconds by default. Each user can pause it on the page." },
    { key: "recent_auto_refresh", label: "Refresh the recent studies automatically", type: "checkbox", wide: true },
    { key: "recently_viewed", label: "Show \"Recently viewed\" on the home page", type: "checkbox", wide: true,
      hint: "Each person's own list of the studies they opened last (in a viewer, the study window, its series). Nobody else sees it." },
    { key: "recently_viewed_count", label: "Recently viewed studies shown", type: "number", min: 1, max: 30 },
    { key: "transfers_auto_refresh", label: "Refresh the Transfers page automatically", type: "checkbox", wide: true, hint: "While a transfer is running, the page follows it every 3 seconds anyway." },
  ],
};

/**
 * The colours of Settings > Appearance: key, label, hint and the colour the picker shows
 * while "Default" is ticked (the built-in light-theme colour).
 */
const COLOR_FIELDS = [
  { key: "color_accent", label: "Accent", fallback: "#2547c8", hint: "Main buttons, selected tabs and filters, links." },
  { key: "color_header", label: "Header", fallback: "#101a24", hint: "The band across the top (logo, name, total of studies). The text colour adapts itself." },
  { key: "color_sidebar", label: "Icon column", fallback: "#ffffff", hint: "The column of icons on the left." },
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
      toast(t("{what} saved.", { what: lookup(title) }));
    } catch (e) { toastError(e); }
    draw();
  }
  async function remove() {
    try {
      await api("DELETE", `/api/admin/branding/${kind}`);
      await refreshBrand();
      toast(t("{what} removed: the default is used again.", { what: lookup(title) }));
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
function buildRolesEditor(roles, defaults, permissions, servers = []) {
  const clone = (list) => list.map((r) => ({ key: r.key, label: r.label, permissions: [...r.permissions], servers: r.servers ? [...r.servers] : null }));
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
    servers.length > 1 ? roleServers(role) : null,
    h("button", { class: "btn small danger", type: "button", onclick: () => { rows.splice(index, 1); draw(); } }, "Remove"))));
  /** "Servers: [x] All  [ ] Main [ ] Archive" of one role (with several Orthanc servers). */
  function roleServers(role) {
    const all = h("input", { type: "checkbox", checked: role.servers == null });
    const boxes = servers.map((server) => h("label", { class: "check" },
      h("input", {
        type: "checkbox", checked: role.servers != null && role.servers.includes(server.key), disabled: role.servers == null,
        onchange: (e) => { role.servers = e.target.checked ? [...new Set([...(role.servers || []), server.key])] : (role.servers || []).filter((k) => k !== server.key); },
      }), server.label));
    all.addEventListener("change", () => { role.servers = all.checked ? null : []; draw(); });
    return h("div", { class: "role-perms role-servers" }, h("span", { class: "muted" }, lookup("Servers:")),
      h("label", { class: "check" }, all, "All servers"), boxes);
  }
  draw();

  const node = h("div", { class: "wide" }, list,
    h("div", { class: "filter-actions" },
      h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ key: "", label: "", permissions: ["read", "visualize"] }); draw(); } }, "Add role"),
      h("button", { class: "btn small", type: "button", onclick: () => { rows = clone(defaults); draw(); } }, "Restore default roles")));

  return { node, get: () => rows.map((r) => ({ key: r.key.trim(), label: r.label.trim(), permissions: r.permissions, servers: r.servers ?? null })) };
}

/**
 * Chooses some items of a catalog and their order (columns, search boxes): one row per item with
 * Shown, ↑ and ↓. `catalog` = [[key, title]]; `get()` = the chosen keys, in order.
 */
function buildPickList(what, catalog, chosen, defaults) { // `what`: "columns", "search boxes"
  const titles = new Map(catalog);
  const make = (keys) => [...keys.filter((k) => titles.has(k)).map((key) => ({ key, on: true })),
    ...catalog.filter(([k]) => !keys.includes(k)).map(([key]) => ({ key, on: false }))];
  let rows = make(chosen);
  const list = h("div", { class: "pick-list" });
  function draw(focus) {
    list.replaceChildren(...rows.map((row, i) => {
      const move = (step, symbol, label) => h("button", {
        class: "btn small", type: "button", "aria-label": `${lookup(label)} ${lookup(titles.get(row.key))}`, title: label, disabled: i + step < 0 || i + step >= rows.length,
        "data-control": symbol,
        onclick: () => { [rows[i], rows[i + step]] = [rows[i + step], rows[i]]; draw([i + step, symbol]); },
      }, symbol);
      return h("div", { class: `pick-row${row.on ? "" : " is-hidden"}`, "data-key": row.key },
        h("div", { class: "modbtn-move" }, move(-1, "↑", "Move up"), move(1, "↓", "Move down")),
        h("label", { class: "check" },
          h("input", { type: "checkbox", checked: row.on, onchange: (e) => { row.on = e.target.checked; draw(); } }),
          titles.get(row.key)));
    }));
    if (focus) {
      const control = list.children[focus[0]] && list.children[focus[0]].querySelector(`[data-control="${focus[1]}"]`);
      if (control && !control.disabled) control.focus();
    }
  }
  draw();
  return {
    node: h("div", { class: "pick-editor" }, list,
      h("div", { class: "filter-actions" }, h("button", { class: "btn small", type: "button", onclick: () => { rows = make(defaults); draw(); } }, t("Restore the default {what}", { what: lookup(what) })))),
    get: () => rows.filter((r) => r.on).map((r) => r.key),
  };
}

/**
 * Editor of the modality buttons of the search pages (Settings > Search & labels): order (move
 * up / down), shown or hidden, label, DICOM codes and description, plus add / remove /
 * restore defaults. A live preview shows the row exactly as users will see it, with the
 * "Other codes" box that is always there. Saved with the rest of the form.
 */
/**
 * A colour picker with a "Default" box: `onChange(color)` gets "#rrggbb", or "" while Default is ticked
 * (the built-in colour). `label` names it for screen readers.
 */
function colorChoice(value, onChange, label) {
  const picker = h("input", { type: "color", value: isColor(value) ? value : "#2547c8", disabled: !isColor(value), "aria-label": label });
  const useDefault = h("input", { type: "checkbox", checked: !isColor(value) });
  const changed = () => { picker.disabled = useDefault.checked; onChange(useDefault.checked ? "" : picker.value); };
  picker.addEventListener("input", changed);
  useDefault.addEventListener("change", changed);
  return h("div", { class: "color-setting" }, picker, h("label", { class: "check" }, useDefault, "Default"));
}

function buildModalityButtonsEditor(buttons, defaults) {
  const clone = (list) => list.map((b) => ({ label: b.label, codes: b.codes.join(", "), title: b.title || "", visible: b.visible !== false, color: b.color || "" }));
  let rows = clone(buttons);
  const parseCodes = (text) => text.split(/[\s,;]+/).map((c) => c.toUpperCase()).filter(Boolean);
  const list = h("div", { class: "modbtn-list" });
  const preview = h("div", { class: "modbtn-preview" });

  function drawPreview() {
    const shown = rows.filter((r) => r.visible && r.label.trim()).map((r) => ({ label: r.label.trim(), codes: parseCodes(r.codes), title: r.title, color: r.color }));
    const picker = buildModalityPicker("", () => {}, shown);
    preview.replaceChildren(h("span", { class: "field-label" }, "Preview (what users see)"), picker.node);
  }

  /** Redraws the rows; `focus` = [row index, control name] to keep the keyboard where it was. */
  function draw(focus) {
    list.replaceChildren(...rows.map((row, index) => {
      const move = (step, name, symbol, label) => h("button", {
        class: "btn small", type: "button", "data-control": name, "aria-label": `${lookup(label)} ${row.label || t("this button")}`, title: label,
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
        colorChoice(row.color, (color) => { row.color = color; drawPreview(); }, t("Colour of the button {label}", { label: row.label || "" })),
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

  // Column titles, aligned with the rows below (the same grid; hidden on narrow screens, where rows wrap).
  // aria-hidden: each control already carries its own accessible name.
  const header = h("div", { class: "modbtn-head", "aria-hidden": "true" },
    h("span", null, "Order"), h("span", null, "Shown"), h("span", null, "Button label"), h("span", null, "Colour"),
    h("span", null, "DICOM codes"), h("span", null, "Description (shown on hover)"), h("span", null, ""));
  const node = h("div", { class: "wide modbtn-editor" }, preview, header, list,
    h("div", { class: "filter-actions" },
      h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ label: "", codes: "", title: "", visible: true, color: "" }); draw([rows.length - 1, "shown"]); } }, "Add button"),
      h("button", { class: "btn small", type: "button", onclick: () => { rows = clone(defaults); draw(); } }, "Restore default buttons")));

  return {
    node,
    get: () => rows.map((r) => ({ label: r.label.trim(), codes: parseCodes(r.codes), title: r.title.trim(), visible: r.visible, color: r.color })),
  };
}

/**
 * Settings > Search & labels > Label colours: one row per label (those in use, the allowed ones, and those
 * already coloured), with a live preview of its tag. `get()` = { label: "#rrggbb" } for the coloured ones.
 */
function buildLabelColorsEditor(colors) {
  const chosen = { ...(colors || {}) };
  const list = h("div", { class: "label-colors" }, h("span", { class: "muted" }, "Loading labels..."));
  const row = (name) => {
    const tag = h("span", { class: "chip label-chip" }, name);
    const paint = (color) => {
      tag.classList.remove("tinted");
      tag.style.removeProperty("--chip");
      tint(tag, color);
    };
    paint(chosen[name]);
    return h("div", { class: "label-color-row" }, tag,
      colorChoice(chosen[name], (color) => { if (color) chosen[name] = color; else delete chosen[name]; paint(color); }, t("Colour of the label {label}", { label: name })));
  };
  const draw = (names) => list.replaceChildren(...(names.length ? names.map(row) : [h("span", { class: "muted" }, "No label yet.")]));
  const allowed = searchOptions().allowed_labels || [];
  // The list the search pages already loaded (asking for it again makes Orthanc count every label).
  loadLabels().then((labels) => draw([...new Set([...labels.map((l) => l.name), ...allowed, ...Object.keys(chosen)])].sort((a, b) => a.localeCompare(b))));
  return { node: list, get: () => ({ ...chosen }) };
}

/**
 * The tabs inside Settings. The first four show parts of ONE form (the settings saved
 * together with the "Save settings" button: `sections` are the ids given to `section()`
 * in buildSettingsView). The others are separate screens whose changes apply at once.
 */
const SETTINGS_TABS = [
  { id: "general", label: "General", sections: ["branding", "viewers"] },
  { id: "appearance", label: "Appearance", sections: ["theme", "simplified", "formatting"] },
  { id: "search", label: "Search & labels", sections: ["search", "lists", "labels"] },
  { id: "features", label: "Features", sections: ["features", "moreviewers"] },
  { id: "orthanc", label: "Orthanc", sections: ["orthanc"] },
  { id: "network", label: "Network & sharing", sections: ["network", "sharing", "email"] },
  { id: "security", label: "Security & roles", sections: ["security", "roles"] },
  { id: "forward", label: "Auto-forward", sections: ["forward"] },
  { id: "backups", label: "Backups", sections: ["backups"] },
  { id: "users", label: "Users", build: buildUsersView },
  { id: "nodes", label: "DICOM nodes", build: buildNodesView },
  { id: "audit", label: "Audit log", build: buildAuditView },
  { id: "jobs", label: "Jobs", build: buildJobsView, when: () => !!opts().jobs_page },
  { id: "system", label: "System", build: buildSystemView, when: () => !!opts().system_page },
  { id: "stats", label: "Statistics", build: buildStatsView, when: () => !!opts().stats_page },
];

/** The "Settings" tab: the complete configuration of the portal, organised in tabs. */
function buildSettingsView() {
  const tabs = SETTINGS_TABS.filter((t) => !t.when || t.when());
  let doc = null; // { settings, listeners, defaults, permissions } from the server
  const noticeSlot = h("div", { class: "notice", role: "status", hidden: true });
  const tabBar = h("div", { class: "subtabs", role: "tablist", "aria-label": "Settings" });
  const formHost = h("div", { class: "view" }, h("div", { class: "panel loading" }, "Loading settings..."));
  const screenHost = h("div", { class: "view" });
  const view = h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Settings")), noticeSlot, tabBar, formHost, screenHost);

  if (!tabs.some((t) => t.id === state.settingsTab)) state.settingsTab = tabs[0].id;

  /**
   * Shows the current tab. Form tabs only hide the other sections, so values typed in one
   * tab are kept (and saved) when switching to another. The other screens are rebuilt each
   * time they are opened, so they always show fresh data.
   */
  const currentTab = () => tabs.find((t) => t.id === state.settingsTab);

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
    const index = tabs.findIndex((t) => t.id === state.settingsTab);
    const step = keys[event.key];
    const next = step === "first" ? 0 : step === "last" ? tabs.length - 1 : (index + step + tabs.length) % tabs.length;
    state.settingsTab = tabs[next].id;
    showTab();
    tabBar.querySelector(`[data-tab="${state.settingsTab}"]`).focus();
  });

  tabs.forEach((t, i) => {
    // A thin divider between the settings form and the separate screens.
    if (i > 0 && t.build && !tabs[i - 1].build) tabBar.append(h("span", { class: "subtabs-divider", "aria-hidden": "true" }));
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
      if (def.type === "list") {
        // A list of words, edited as "a, b, c"; sent to the server as an array.
        const area = h("textarea", { rows: 2, placeholder: def.placeholder }, (value || []).join(", "));
        inputs[def.key] = { type: "list", get value() { return area.value.split(/[\s,;]+/).filter(Boolean); } };
        return h("div", { class: def.wide ? "setting wide" : "setting" }, h("label", null, def.label, area), def.hint ? h("p", { class: "hint" }, def.hint) : null);
      }
      if (def.type === "textarea") {
        input = h("textarea", { rows: def.rows || 3, maxlength: def.maxlength || 500, placeholder: def.placeholder }, value);
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

    /** A titled card of the form. `id` says which tab shows it (tabs). */
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
      testBox.textContent = t("Testing...");
      try {
        const r = await api("POST", "/api/admin/orthanc/test", {
          // Without an account the user name is sent empty; the server then ignores any saved password.
          url: inputs.orthanc_url.value, username: accountName(), password: withoutAccount() ? "" : password.value,
        });
        testBox.className = `test-result ${r.ok ? "ok" : "bad"}`;
        const details = r.version
          ? " " + t("Plugins: {plugins}. DICOMweb: {dicomweb}. OHIF: {ohif}. DICOM nodes: {nodes}.", {
            plugins: r.plugins.join(", ") || t("none"), dicomweb: t(r.dicomweb ? "yes" : "no"), ohif: t(r.ohif ? "yes" : "no"), nodes: r.modalities,
          })
          : "";
        testBox.textContent = translateMessage(r.message) + (r.ok ? "." : "") + details;
      } catch (e) {
        testBox.className = "test-result bad";
        testBox.textContent = e.message;
      } finally {
        testButton.disabled = false;
      }
    });

    const rolesEditor = buildRolesEditor(s.roles, doc.defaults.roles, doc.permissions,
      [{ key: "main", label: s.orthanc_label || "Main" }, ...(s.orthanc_servers || []).map((x) => ({ key: x.key, label: x.label || x.key }))]);
    const forwardEditor = buildForwardRulesEditor(s.forward_rules || [], doc.destinations || []);

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
      testMailBox.textContent = t("Sending...");
      try {
        const r = await api("POST", "/api/admin/email/test", { to: testTo.value });
        testMailBox.className = `test-result ${r.ok ? "ok" : "bad"}`;
        testMailBox.textContent = translateMessage(r.message);
      } catch (e) {
        testMailBox.className = "test-result bad";
        testMailBox.textContent = e.message;
      } finally {
        testMailButton.disabled = false;
      }
    });
    // Microsoft 365: the client secret is write-only too.
    const oauthSecret = h("input", { type: "password", autocomplete: "new-password", placeholder: s.smtp_oauth_client_secret_set ? "A secret is saved. Leave empty to keep it." : "No secret saved" });
    const oauthSecretField = h("div", { class: "setting" },
      h("label", null, "Client secret (its Value)", oauthSecret),
      h("p", { class: "hint" }, "Entra ID > App registrations > your app > Certificates & secrets. Secrets expire: put the new one here before the old one ends."));
    const emailNodes = {};
    for (const def of SETTINGS_FIELDS.email) emailNodes[def.key] = field(def);
    const emailFields = [];
    for (const def of SETTINGS_FIELDS.email) {
      emailFields.push(emailNodes[def.key]);
      if (def.key === "smtp_username") emailFields.push(smtpPasswordField);
      if (def.key === "smtp_oauth_client_id") emailFields.push(oauthSecretField);
    }
    // Show what the chosen sign-in needs; choosing Microsoft 365 fills in its mail server.
    const smtpAuth = inputs.smtp_auth;
    const smtpUserLabel = emailNodes.smtp_username.querySelector("label").firstChild;
    const smtpUserHint = emailNodes.smtp_username.querySelector(".hint");
    const showSignIn = () => {
      const m365 = smtpAuth.value === "microsoft365";
      for (const def of SETTINGS_FIELDS.email) if (def.m365) emailNodes[def.key].hidden = !m365;
      oauthSecretField.hidden = !m365;
      smtpPasswordField.hidden = m365;
      smtpUserLabel.textContent = t(m365 ? "Mailbox the portal sends from" : "User name on the mail server");
      if (smtpUserHint) smtpUserHint.textContent = t(m365
        ? "A Microsoft 365 mailbox (a shared mailbox is fine), e.g. pacs@example.com. Use the same address as the sender below."
        : "Empty when the server accepts the portal without signing in (a relay on your network).");
    };
    smtpAuth.addEventListener("change", () => {
      if (smtpAuth.value === "microsoft365" && !inputs.smtp_host.value.trim()) {
        inputs.smtp_host.value = "smtp.office365.com";
        inputs.smtp_port.value = 587;
        inputs.smtp_security.value = "starttls";
      }
      showSignIn();
    });
    showSignIn();
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
      const picker = h("input", { type: "color", value: saved || def.fallback, disabled: !saved, "aria-label": t("{name} colour", { name: lookup(def.label) }) });
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
    const labelColorsEditor = buildLabelColorsEditor(s.label_colors);
    const columnsEditor = buildPickList("columns", Object.entries(STUDY_COLUMNS).map(([k, c]) => [k, c.title]), s.study_columns, doc.defaults.study_columns);
    const fieldsEditor = buildPickList("search boxes", Object.entries(STUDY_FIELDS).map(([k, f]) => [k, f.title]), s.study_search_fields, doc.defaults.study_search_fields);
    const detailEditor = buildPickList("fields", Object.entries(STUDY_DETAIL_TITLES), s.study_detail_fields || [], doc.defaults.study_detail_fields || []);
    const ohifModesEditor = buildPickList("modes", Object.entries(OHIF_MODES).map(([k, [title]]) => [k, title]), s.ohif_modes || ["viewer"], ["viewer"]);

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
      h("li", null, h("b", null, l.name), ": ", l.running ? t("listening on {address}", { address: l.addr }) : "not running")));

    // ---- Notices: what the last save did, and a reconnect link if this page's port is no longer the intranet port
    const notices = [];
    messages.forEach((m) => notices.push(h("p", null, translateMessage(m))));
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
        // Collect the values of every control into the object the server expects.
        const body = {};
        for (const [key, input] of Object.entries(inputs)) {
          body[key] = input.type === "checkbox" ? input.checked : input.type === "number" ? Number(input.value) : input.value;
        }
        body.roles = rolesEditor.get();
        body.modality_buttons = modalityEditor.get();
        body.label_colors = labelColorsEditor.get();
        body.study_columns = columnsEditor.get();
        body.study_search_fields = fieldsEditor.get();
        body.study_detail_fields = detailEditor.get();
        body.ohif_modes = ohifModesEditor.get();
        body.forward_rules = forwardEditor.get();
        if (smtpPassword.value) body.smtp_password = smtpPassword.value;
        if (oauthSecret.value) body.smtp_oauth_client_secret = oauthSecret.value;
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
          const languageChanged = await refreshBrand();
          // Viewer buttons, thumbnails and the "Share..." dialog follow the features: take the
          // server's fresh view of them (it also knows whether Orthanc has the OHIF plugin), so
          // what was just saved applies right away, without signing in again.
          // A changed role can also change your own permissions (the menu and the buttons follow them).
          let permissionsChanged = false;
          try {
            const me = await api("GET", "/api/me");
            permissionsChanged = [...me.permissions].sort().join(",") !== [...state.user.permissions].sort().join(",");
            if (permissionsChanged) state.user = me; else state.user.features = me.features;
          } catch { /* keep the old ones */ }
          refreshOrthancStatus(true);
          toast(t("Settings saved."));
          if (languageChanged || permissionsChanged) { renderApp(); return; } // everything in the new language / menu
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
      section("lists", "Studies lists and search", "Which columns the studies tables show (Search and home page), which boxes the Studies search offers, in which order, and how searching and paging behave. A column with its title underlined can be sorted by clicking it.",
        h("div", { class: "setting" }, h("span", { class: "field-label" }, "Columns of the studies tables"), columnsEditor.node),
        h("div", { class: "setting" }, h("span", { class: "field-label" }, "Boxes of the Studies search"), fieldsEditor.node),
        h("div", { class: "setting" }, h("span", { class: "field-label" }, "Fields of the study window (a click on a card)"), detailEditor.node),
        SETTINGS_FIELDS.lists.map(field)),
      section("labels", "Labels", "Short words staff put on studies to find them again (urgent, insurance, teaching...). Everyone who can search sees them and can search by them; people with the labels permission (and administrators) put them on and take them off. They are Orthanc's own labels.",
        SETTINGS_FIELDS.labels.map(field),
        h("div", { class: "setting wide" }, h("span", { class: "field-label" }, "Label colours"),
          h("p", { class: "hint" }, "The colour of each label's tags and filter button. \"Default\" keeps the accent colour. The colours adapt to the light and the dark theme."),
          labelColorsEditor.node)),
      section("theme", "Colours", "Match the portal to your practice's colours. Changes are previewed on this page at once and kept when you save. The logo, the portal name and the window title are in the General tab.",
        themeFields),
      section("simplified", "Home page and icons", "The portal shows the logo, the practice name and the total of studies at the top, a column of icons (Home, Search, Shares, Uploads, Transfers, Help, Configuration, Log out) on the left, and a home page with the most recent studies and an area to drop files into. Each person sees only the icons their permissions allow.",
        SETTINGS_FIELDS.simplified.map(field)),
      section("formatting", "Language, dates, names and style", "The language of the portal, how dates and patient names are written everywhere (also on the invitation page), and extra style rules.",
        SETTINGS_FIELDS.formatting.map(field)),
      section("features", "Optional features", "Switch on only what your practice needs. Each feature also needs its permission (Security & roles, Users): delete, edit, retrieve, worklists... Administrators may use all of them.",
        SETTINGS_FIELDS.features.map(field)),
      section("moreviewers", "More viewers", "Viewers of Orthanc's plugins, offered in a \"More viewers\" menu next to OHIF, and the OHIF modes. Several ticked studies can be opened together in OHIF and Stone.",
        SETTINGS_FIELDS.moreviewers.map(field),
        h("div", { class: "setting" }, h("span", { class: "field-label" }, "OHIF modes offered"), ohifModesEditor.node)),
      section("orthanc", "Orthanc connection", "The PACS this portal works with. Saved changes apply immediately, without restarting.",
        urlField, modeField, userField, passwordField,
        h("div", { class: "setting wide" }, testButton, testBox),
        field({ key: "orthanc_log_dir", label: "Folder of Orthanc's log files (optional)", wide: true,
          placeholder: "/var/log/orthanc   or   C:\\Program Files\\Orthanc Server\\Logs",
          hint: "Where Orthanc writes its log (its --logdir option), as seen from the computer or container running the portal. Orthanc cannot send its log over its API: with this folder, Configuration > System shows it. Empty = not shown." })),
      section("network", "Network", networkIntro, networkFields, listenerInfo),
      section("sharing", "Share links (invitations for people outside)", "A staff member with the share permission can invite somebody outside: a link plus a generated password that opens ONE study, to look at online and to download a limited number of times, until the link expires. These are the defaults offered when creating a link, and the limits nobody can exceed.", SETTINGS_FIELDS.sharing.map(field)),
      section("email", "Email (sending share invitations)", "The Share windows can send the invitation by email (optional, for each link) when a mail server is set here. The emails are plain text; the password is included only when the staff member ticks it.",
        emailFields, testMailField),
      section("security", "Sessions and sign-in security", null, SETTINGS_FIELDS.security.map(field)),
      section("forward", "Auto-forward", "Send the studies that arrive on to another system automatically, for example every CT from the scanner to the reporting workstation, or be told by email, for example when a CT from a given centre or a study labelled urgent arrives. A rule matches when every condition filled in holds; empty conditions match everything. The transfers appear on the Transfers page as \"auto: <rule>\".",
        SETTINGS_FIELDS.forward.map(field), forwardEditor.node, forwardStatus(doc.forward)),
      section("backups", "Backups of the portal's database", "Users, settings, the audit log and the links (the images are in Orthanc, which has its own backups). A copy is written to the folder every few hours; you can also make one now or download one. To restore: stop the portal, put the copy in place of the database file, start it again.",
        SETTINGS_FIELDS.backups.map(field), buildBackupsPanel()),
      section("roles", "Roles", "Bundles of permissions given to users (Users tab). Users follow their role: changing a role's permissions here changes the permissions of every user with that role when you save (signed-in people too, without signing them out). Users with \"Custom\" permissions are not changed.", rolesEditor.node),
      h("div", { class: "settings-actions" },
        save,
        h("span", { class: "muted" }, "Saves the General, Appearance, Search & labels, Features, Orthanc, Network & sharing and Security & roles, Auto-forward and Backups tabs together.")));

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
              try { await api("POST", `/api/admin/nodes/${encodeURIComponent(n.id)}/echo`); toast(t("{node} answered the DICOM ping.", { node: n.id })); } catch (e) { toastError(e); }
              button.disabled = false;
            },
          }, "Test"),
          h("button", { class: "btn small", type: "button", onclick: () => openNodeDialog(n, load) }, "Edit"),
          h("button", {
            class: "btn small danger", type: "button",
            onclick: async () => {
              if (!confirm(t('Delete the DICOM node "{node}"?', { node: n.id }))) return;
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
    h("h2", null, isNew ? "Add a DICOM node" : t("Edit {name}", { name: node.id })),
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
   9b. OPTIONAL FEATURES (Settings > Features)
   Each part shows only when an administrator switched it on AND the user may use it (the
   server sends both in features.options; it enforces them too).
   ==================================================================== */

/** The optional features of this user (features.options), the other viewers (features.viewers). */
const opts = () => (state.user && state.user.features && state.user.features.options) || {};
const viewersOpt = () => (state.user && state.user.features && state.user.features.viewers) || {};
/** Labels can be switched off altogether (Settings > Search & labels). */
const labelsOn = () => opts().labels !== false;

/** A small "..." menu (details element) of a study or series: `items` = [label, onclick, danger?]. */
function actionMenu(iconName, name, tip, items) {
  const list = items.filter(Boolean);
  if (!list.length) return null;
  return h("details", { class: "menu" },
    actionButton("summary", iconName, name, tip, { class: "btn small" }),
    h("div", { class: "menu-list" }, list.map(([label, onclick, danger]) => h("button", {
      type: "button", class: danger ? "menu-item danger-text" : "menu-item",
      onclick: (event) => { closeMenu(event); onclick(); },
    }, label))));
}

/** Something changed in the archive (deleted, modified...): the lists on the page search again. */
function archiveChanged() {
  if (typeof reloadRecentStudies === "function") reloadRecentStudies();
  window.dispatchEvent(new Event("portal-archive-changed"));
  totalStudies.at = 0;
  refreshTotalStudies(true);
}

/**
 * Follows an Orthanc job (modify, anonymize...) until it ends, then says so and refreshes the
 * lists. Orthanc does the work in the background: leaving the page does not stop it.
 */
async function followJob(jobId, what) {
  toast(t("{what}: started. It continues even if you leave the page.", { what: lookup(what) }), "info", 5000);
  for (let i = 0; i < 600; i++) {
    await new Promise((r) => setTimeout(r, i < 5 ? 1000 : 3000));
    let job;
    try { job = await api("GET", `/api/jobs/${encodeURIComponent(jobId)}`); } catch (e) { toastError(e); return; }
    if (job.state === "Success") { toast(t("{what}: done.", { what: lookup(what) }), "info", 6000); archiveChanged(); return; }
    if (job.state === "Failure") { toast(t("{what} failed: {error}", { what: lookup(what), error: job.error ? translateMessage(job.error) : t("see the Jobs page") }), "error", 10000); return; }
  }
}

/** A simple window with a form: `fields` are nodes, `submit(close)` does the work. */
function formDialog(title, intro, fields, submitLabel, submit, danger = false) {
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const button = h("button", { class: danger ? "btn danger" : "btn primary", type: "submit" }, submitLabel);
  const dialog = h("dialog", null, h("form", {
    class: "dialog-form",
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      button.disabled = true;
      try { await submit(); dialog.close(); } catch (e) { error.textContent = e.message; error.hidden = false; button.disabled = false; }
    },
  },
    h("h2", null, title), intro ? h("p", { class: "muted" }, intro) : null, error, fields,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Cancel"), button)));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  return dialog;
}

/** "DOE, JOHN 2024-05-17, CT CHEST": a study in one line. */
const studyLine = (s) => [formatName(s.patient_name), formatDicomDate(s.study_date), s.study_description].filter(Boolean).join("  ");

/* ---- More viewers ---- */

/** The OHIF modes: button text and address (the study UIDs are added at the end). */
const OHIF_MODES = {
  viewer: ["OHIF", "/ohif/viewer?StudyInstanceUIDs="],
  mpr: ["OHIF: MPR", "/ohif/viewer?hangingprotocolId=mpr&StudyInstanceUIDs="],
  volume: ["OHIF: 3D volume", "/ohif/viewer?hangingprotocolId=mprAnd3DVolumeViewport&StudyInstanceUIDs="],
  tmtv: ["OHIF: total metabolic tumor volume", "/ohif/tmtv?StudyInstanceUIDs="],
  segmentation: ["OHIF: segmentation", "/ohif/segmentation?StudyInstanceUIDs="],
  microscopy: ["OHIF: microscopy", "/ohif/microscopy?StudyInstanceUIDs="],
};

/**
 * Every browser viewer that can open these studies: [text, address]. Several studies open
 * together in OHIF and Stone; VolView and MedDream take one study.
 */
function viewerLinks(studies) {
  const v = viewersOpt();
  const uids = studies.map((s) => s.study_instance_uid).filter(Boolean);
  if (!uids.length) return [];
  const list = [];
  if (hasOhif()) {
    for (const mode of v.ohif_modes || ["viewer"]) {
      // Studies opened together are on one server (the first one's): OHIF reads them all there.
      if (OHIF_MODES[mode]) list.push([OHIF_MODES[mode][0], OHIF_MODES[mode][1] + uids.map(encodeURIComponent).join(",") + serverParam(studies[0].server)]);
    }
  }
  if (v.stone) list.push(["Stone Web Viewer", `/stone-webviewer/index.html?study=${uids.map(encodeURIComponent).join(",")}`]);
  if (studies.length === 1) {
    if (v.volview && can("download")) list.push(["VolView (3D)", `/volview/index.html?names=[archive.zip]&urls=[../api/studies/${studies[0].id}/download]`]);
    if (v.meddream_url) list.push(["MedDream", v.meddream_url.replaceAll("{study_uid}", encodeURIComponent(uids[0]))]);
  }
  return list;
}

/** The viewers beyond the main OHIF button, as a menu of links (new tab each). */
function moreViewersMenu(studies) {
  const links = viewerLinks(studies).filter(([text]) => text !== "OHIF");
  if (!links.length) return null;
  return h("details", { class: "menu" },
    actionButton("summary", "viewers", "More viewers", "Open in another viewer", { class: "btn small" }),
    h("div", { class: "menu-list" }, links.map(([text, href]) =>
      h("a", { class: "menu-item", href, target: "_blank", rel: "noopener", onclick: closeMenu }, text))));
}

/* ---- Bulk actions: tick several studies ---- */

/** Can this user do anything with several studies at once? Asked for every row: worked out
 *  once per sign-in / settings change (a new features object), then remembered. */
let bulkMemo = { features: null, value: false };
function bulkOn() {
  const features = state.user && state.user.features;
  if (bulkMemo.features !== features) {
    const value = !!opts().bulk && (can("download") || can("send") || can("share") || canEditLabels() || !!opts().delete
      || (can("visualize") && viewerLinks([{ study_instance_uid: "1" }]).length > 0));
    bulkMemo = { features, value };
  }
  return bulkMemo.value;
}

/** The ticked studies of the list that contains `node`. */
const pickedStudies = (block) => Array.from(block.querySelectorAll("input.pick:checked")).map((box) => box.study).filter(Boolean);

/** The bar above a studies table: "3 selected" and what can be done with them. */
function buildBulkBar(block) {
  const bar = h("div", { class: "bulk-bar", hidden: true });
  const draw = () => {
    const picked = pickedStudies(block);
    bar.hidden = picked.length === 0;
    if (!picked.length) return;
    const clear = () => { block.querySelectorAll("input.pick").forEach((b) => { b.checked = false; }); draw(); };
    const viewers = can("visualize") ? viewerLinks(picked) : [];
    bar.replaceChildren(
      h("b", null, t("{n} selected", { n: picked.length })),
      viewers.length ? h("details", { class: "menu" },
        h("summary", { class: "btn small" }, "Open together"),
        h("div", { class: "menu-list" }, viewers.map(([text, href]) => h("a", { class: "menu-item", href, target: "_blank", rel: "noopener", onclick: closeMenu }, text)))) : null,
      can("download") ? h("details", { class: "menu" },
        h("summary", { class: "btn small", "data-name": "Download selected" }, "Download (one ZIP)"),
        h("div", { class: "menu-list" },
          h("a", { class: "menu-item", href: `/api/download?studies=${picked.map((s) => s.id).join(",")}`, onclick: closeMenu }, "ZIP", h("small", null, "DICOM files in folders")),
          h("a", { class: "menu-item", href: `/api/download?studies=${picked.map((s) => s.id).join(",")}&dicomdir=true`, onclick: closeMenu }, "ZIP with DICOMDIR", h("small", null, "Adds an index, like a CD/DVD")),
          cdExportItem(picked.map((s) => s.id)))) : null,
      can("send") ? h("button", { class: "btn small", type: "button", onclick: () => openSendDialog(picked) }, "Send...") : null,
      can("share") && (opts().multi_share || picked.length === 1) ? h("button", { class: "btn small", type: "button", onclick: () => openShareDialog(picked) }, "Share (one link)...") : null,
      canEditLabels() ? h("button", { class: "btn small", type: "button", onclick: () => openBulkLabelDialog(picked) }, "Labels...") : null,
      opts().delete ? h("button", { class: "btn small danger", type: "button", onclick: () => deleteStudies(picked, clear) }, "Delete...") : null,
      h("button", { class: "btn small", type: "button", onclick: clear }, "Clear"));
  };
  block.addEventListener("change", (event) => { if (event.target.classList.contains("pick")) draw(); });
  return bar;
}

/** The tick box of one study row (it carries the study). */
function pickBox(study) {
  const box = h("input", { type: "checkbox", class: "pick", "aria-label": t("Select {name}", { name: formatName(study.patient_name) }) });
  box.study = study;
  return box;
}

/** "Select all" of a table head. */
function pickAllBox() {
  return h("input", {
    type: "checkbox", class: "pick-all", "aria-label": "Select all",
    onchange: (event) => {
      const block = event.target.closest(".studies-block");
      block.querySelectorAll("input.pick").forEach((b) => { b.checked = event.target.checked; });
      const first = block.querySelector("input.pick");
      if (first) first.dispatchEvent(new Event("change", { bubbles: true })); // redraws the bar
    },
  });
}

/** Puts one label on (or takes it off) every selected study. */
function openBulkLabelDialog(studies) {
  const allowed = searchOptions().allowed_labels || [];
  const label = h("input", { required: true, maxlength: 64, list: "bulk-label-list", placeholder: "urgent" });
  const datalist = h("datalist", { id: "bulk-label-list" });
  loadLabels().then((list) => datalist.replaceChildren(...[...new Set([...allowed, ...list.map((l) => l.name)])].map((n) => h("option", { value: n }))));
  const action = h("select", null, h("option", { value: "add" }, "Put this label on them"), h("option", { value: "remove" }, "Take this label off"));
  formDialog(t("Labels of {n} studies", { n: studies.length }), null,
    [h("label", null, "Label", label, datalist), h("label", null, "What to do", action)], "Apply", async () => {
      const name = label.value.trim();
      for (const s of studies) {
        const labels = await api(action.value === "add" ? "PUT" : "DELETE", `/api/studies/${s.id}/labels/${encodeURIComponent(name)}`);
        s.labels = labels.labels || labels;
      }
      labelsCache = null;
      toast(t(action.value === "add" ? "Label {label} put on {n} studies." : "Label {label} taken off {n} studies.", { label: name, n: studies.length }));
      archiveChanged();
    });
}

/** Deletes studies after a clear confirmation. */
function deleteStudies(studies, done = () => {}) {
  const many = studies.length > 1;
  formDialog(many ? t("Delete {n} studies?", { n: studies.length }) : "Delete this study?",
    "The images are removed from the archive for good. This cannot be undone.",
    h("ul", null, studies.slice(0, 10).map((s) => h("li", null, studyLine(s))), studies.length > 10 ? h("li", null, t("and {n} more", { n: studies.length - 10 })) : null),
    many ? t("Delete {n} studies", { n: studies.length }) : "Delete the study", async () => {
      for (const s of studies) await api("DELETE", `/api/studies/${s.id}`);
      toast(many ? t("{n} studies deleted.", { n: studies.length }) : "Study deleted.");
      done();
      archiveChanged();
    }, true);
}

/* ---- Images, DICOM tags, PDF reports ---- */

/** Opens the first PDF report of a study (its DOC series). */
async function openStudyPdf(study, button) {
  if (button) button.disabled = true;
  const tab = window.open("about:blank", "_blank");
  try {
    const series = await api("GET", `/api/studies/${study.id}/series`);
    for (const s of series.filter((x) => x.modality === "DOC")) {
      const images = await api("GET", `/api/series/${s.id}/instances`);
      const pdf = images.find((i) => i.is_pdf);
      if (pdf) { if (tab) tab.location.href = `/api/instances/${pdf.id}/pdf`; return; }
    }
    if (tab) tab.close();
    toast("This study has no PDF report.", "info");
  } catch (e) {
    if (tab) tab.close();
    toastError(e);
  } finally {
    if (button) button.disabled = false;
  }
}

/** The images of a series: a list on the left, the picture (or the PDF / tags) on the right. */
async function openInstancesDialog(series) {
  let images;
  try { images = await api("GET", `/api/series/${series.id}/instances`); } catch (e) { toastError(e); return; }
  const viewer = h("div", { class: "instance-view" }, h("p", { class: "muted" }, "Choose an image."));
  const dialog = h("dialog", { class: "wide-dialog" });

  function showImage(image) {
    const actions = h("div", { class: "actions" },
      image.is_pdf && can("visualize") ? h("a", { class: "btn small", href: `/api/instances/${image.id}/pdf`, target: "_blank", rel: "noopener" }, "Open the PDF") : null,
      h("button", { class: "btn small", type: "button", onclick: () => showTags(image) }, "DICOM tags"),
      can("download") && opts().single_file ? h("a", { class: "btn small", href: `/api/instances/${image.id}/download` }, "Download this file") : null,
      opts().delete ? h("button", {
        class: "btn small danger", type: "button",
        onclick: () => formDialog("Delete this image?", "It is removed from the archive for good.", null, "Delete the image", async () => {
          await api("DELETE", `/api/instances/${image.id}`);
          toast("Image deleted.");
          dialog.close();
          archiveChanged();
        }, true),
      }, "Delete") : null);
    const picture = h("div", { class: "instance-picture" });
    if (can("visualize") && !image.is_pdf) {
      picture.append(h("div", { class: "loading" }, "Loading the picture..."));
      fetch(`/api/instances/${image.id}/preview?width=1024`, { credentials: "same-origin" })
        .then(async (r) => {
          if (!r.ok) throw new Error(translateMessage((await r.json().catch(() => ({}))).error) || t("No picture for this image."));
          const url = URL.createObjectURL(await r.blob());
          picture.replaceChildren(h("img", { src: url, alt: t("Image {n}", { n: image.number || "" }) }));
        })
        .catch((e) => picture.replaceChildren(h("p", { class: "muted" }, e.message)));
    } else if (image.is_pdf) {
      picture.append(h("p", { class: "muted" }, "A PDF report."));
    }
    viewer.replaceChildren(h("h3", null, t("Image {n}", { n: image.number || "" })), actions, picture);
  }

  async function showTags(image) {
    viewer.replaceChildren(h("div", { class: "loading" }, "Loading the tags..."));
    try {
      const tags = await api("GET", `/api/instances/${image.id}/tags`);
      const filter = h("input", { placeholder: "Filter: name, tag or value", "aria-label": "Filter the tags" });
      const body = h("tbody");
      const rows = flattenTags(tags);
      const draw = () => {
        const q = filter.value.trim().toLowerCase();
        body.replaceChildren(...rows.filter((r) => !q || r.join(" ").toLowerCase().includes(q)).map(([tag, name, value, depth]) =>
          h("tr", null, h("td", { class: "mono nowrap" }, tag), h("td", { class: depth ? "tag-nested" : null }, name), h("td", { class: "tag-value" }, value))));
      };
      filter.addEventListener("input", draw);
      draw();
      viewer.replaceChildren(h("h3", null, t("DICOM tags of image {n}", { n: image.number || "" })),
        h("div", { class: "actions" }, filter, h("button", { class: "btn small", type: "button", onclick: () => showImage(image) }, "Back to the image")),
        h("div", { class: "table-wrap tags-table" }, h("table", { class: "data" },
          h("thead", null, h("tr", null, h("th", null, "Tag"), h("th", null, "Name"), h("th", null, "Value"))), body)));
    } catch (e) {
      viewer.replaceChildren(h("p", { class: "form-error" }, e.message));
    }
  }

  const list = h("div", { class: "instance-list" }, images.length ? images.map((image) => h("button", {
    type: "button", class: "btn small", onclick: () => showImage(image),
  }, image.is_pdf ? `PDF ${image.number || ""}` : t("Image {n}", { n: image.number || "?" }))) : h("p", { class: "muted" }, "No image."));
  dialog.append(h("div", { class: "dialog-form" },
    h("h2", null, t("{series} ({modality}): {n} images", { series: series.description || t("Series"), modality: series.modality || "?", n: images.length })),
    h("div", { class: "instances" }, list, viewer),
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", type: "button", onclick: () => dialog.close() }, "Close"))));
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  if (images.length) showImage(images[0]);
}

/** Orthanc's `/tags` (nested sequences) as rows [tag, name, value, depth]. */
function flattenTags(tags, depth = 0, out = []) {
  for (const [tag, item] of Object.entries(tags || {})) {
    const name = `${"  ".repeat(depth)}${item.Name || ""}`;
    if (item.Type === "Sequence") {
      out.push([tag, name, t("{n} item(s)", { n: (item.Value || []).length }), depth]);
      (item.Value || []).forEach((child) => flattenTags(child, depth + 1, out));
    } else {
      const value = item.Type === "String" ? item.Value : item.Type === "TooLong" ? t("(too long to show)") : item.Type === "Null" ? "" : `(${item.Type || "binary"})`;
      out.push([tag, name, value == null ? "" : String(value), depth]);
    }
  }
  return out;
}

/* ---- Modify, move, anonymize, add a series ---- */

/** The tags that can be changed, as [DICOM keyword, label, value of the study / series]. */
const STUDY_EDIT_TAGS = [
  ["PatientName", "Patient name (DOE^JOHN)", (s) => s.patient_name], ["PatientID", "Patient ID", (s) => s.patient_id],
  ["PatientBirthDate", "Birth date (YYYYMMDD)", (s) => s.patient_birth_date], ["PatientSex", "Sex (M, F, O)", (s) => s.patient_sex],
  ["OtherPatientIDs", "Other patient IDs", (s) => s.other_patient_ids], ["StudyDescription", "Study description", (s) => s.study_description],
  ["StudyDate", "Study date (YYYYMMDD)", (s) => s.study_date], ["AccessionNumber", "Accession number", (s) => s.accession_number],
  ["ReferringPhysicianName", "Referring physician", (s) => s.referring_physician], ["RequestingPhysician", "Requesting physician", (s) => s.requesting_physician],
  ["InstitutionName", "Institution", (s) => s.institution],
];
const SERIES_EDIT_TAGS = [
  ["SeriesDescription", "Series description", (s) => s.description], ["SeriesNumber", "Series number", (s) => s.series_number],
  ["BodyPartExamined", "Body part", (s) => s.body_part],
];

/** Changes tags of a study or series (only the boxes that were changed are sent). */
function openModifyDialog(kind, item) {
  const tags = kind === "studies" ? STUDY_EDIT_TAGS : SERIES_EDIT_TAGS;
  const inputs = tags.map(([key, label, get]) => {
    const input = h("input", { value: get(item) || "", maxlength: 64, "data-tag": key });
    input.original = get(item) || "";
    return h("label", null, label, input);
  });
  const keep = h("input", { type: "checkbox" });
  formDialog(kind === "studies" ? "Change the study" : "Change the series",
    "The images are rewritten with the new values; the original is replaced. Other systems that already have this study keep the old values.",
    [h("div", { class: "settings-grid" }, inputs),
      h("label", { class: "check" }, keep, "Keep the DICOM identifiers (UIDs): it stays \"the same study\" for other systems")],
    "Save the changes", async () => {
      const replace = {};
      for (const label of inputs) {
        const input = label.querySelector("input");
        if (input.value.trim() !== input.original) replace[input.dataset.tag] = input.value.trim();
      }
      if (!Object.keys(replace).length) throw new Error(t("Nothing was changed."));
      const r = await api("POST", `/api/${kind}/${item.id}/modify`, { replace, keep_uids: keep.checked });
      followJob(r.job_id, kind === "studies" ? "Changing the study" : "Changing the series");
    });
}

/** Moves a study to another patient: search that patient's studies, pick one. */
function openMoveStudyDialog(study) {
  const query = h("input", { placeholder: "Patient name or ID", "aria-label": "Find the patient" });
  const results = h("div", { class: "move-results" }, h("p", { class: "muted" }, "Search the patient the study belongs to."));
  let chosen = null;
  const find = async () => {
    const v = query.value.trim();
    if (!v) return;
    results.replaceChildren(h("div", { class: "loading" }, "Searching..."));
    try {
      const [byName, byId] = await Promise.all([
        api("GET", `/api/studies?patient_name=${encodeURIComponent(v)}&limit=20`), api("GET", `/api/studies?patient_id=${encodeURIComponent(v)}&limit=20`)]);
      const seen = new Map();
      [...byName.items, ...byId.items].filter((s) => s.patient_id !== study.patient_id).forEach((s) => { if (!seen.has(s.patient_id)) seen.set(s.patient_id, s); });
      results.replaceChildren(...(seen.size ? [...seen.values()].map((s) => h("label", { class: "check" },
        h("input", { type: "radio", name: "move-target", onchange: () => { chosen = s; } }),
        `${formatName(s.patient_name)} (${s.patient_id})`)) : [h("p", { class: "muted" }, "No other patient found.")]));
    } catch (e) { results.replaceChildren(h("p", { class: "form-error" }, e.message)); }
  };
  query.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); find(); } });
  formDialog("Move the study to another patient", studyLine(study),
    [h("div", { class: "actions" }, query, h("button", { class: "btn small", type: "button", onclick: find }, "Find")), results],
    "Move the study", async () => {
      if (!chosen) throw new Error(t("Choose the patient first."));
      const r = await api("POST", `/api/studies/${study.id}/move`, { patient_from_study: chosen.id });
      followJob(r.job_id, "Moving the study");
    });
}

/** Moves a series to another study of the same patient, or to a new study. */
async function openMoveSeriesDialog(series, study) {
  let others = [];
  try { others = (await api("GET", `/api/studies?patient_id=${encodeURIComponent(`"${study.patient_id}"`)}&limit=50`)).items.filter((s) => s.id !== study.id); } catch { /* only "new study" */ }
  const target = h("select", null, h("option", { value: "new" }, "A new study of this patient"), others.map((s) => h("option", { value: s.id }, studyLine(s))));
  const description = h("input", { maxlength: 64, placeholder: "Description of the new study" });
  target.addEventListener("change", () => { description.parentElement.hidden = target.value !== "new"; });
  formDialog("Move the series", `${series.description || t("Series")} (${series.modality})`,
    [h("label", null, "Move it to", target), h("label", null, "Description of the new study", description)],
    "Move the series", async () => {
      const r = await api("POST", `/api/series/${series.id}/move`, { study: target.value, description: description.value });
      followJob(r.job_id, "Moving the series");
    });
}

/** Anonymizes a study or a series (a copy, by default). */
function openAnonymizeDialog(kind, item) {
  const name = h("input", { maxlength: 64, placeholder: "Anonymous (empty = generated)" });
  const id = h("input", { maxlength: 64, placeholder: "Empty = generated" });
  const keep = h("input", { type: "checkbox", checked: true });
  formDialog(kind === "studies" ? "Anonymize the study" : "Anonymize the series",
    "Names, IDs, dates of birth and the other identifying tags are removed or replaced. The anonymized copy appears as a new study.",
    [h("label", null, "New patient name", name), h("label", null, "New patient ID", id), h("label", { class: "check" }, keep, "Keep the original too")],
    "Anonymize", async () => {
      const r = await api("POST", `/api/${kind}/${item.id}/anonymize`, { patient_name: name.value, patient_id: id.value, keep_source: keep.checked });
      followJob(r.job_id, "Anonymizing");
    });
}

/** Adds a series made of PDF files or pictures to a study. */
function openAddSeriesDialog(study) {
  const files = h("input", { type: "file", multiple: true, accept: "application/pdf,image/png,image/jpeg", required: true });
  const description = h("input", { maxlength: 64, placeholder: "Report, Photos..." });
  formDialog("Add a series to the study", t("{study}. PDF files become reports (one per file); PNG and JPEG pictures become one series of images.", { study: studyLine(study) }),
    [h("label", null, "Files", files), h("label", null, "Series description", description)], "Add the series", async () => {
      const list = Array.from(files.files || []);
      if (!list.length) throw new Error(t("Choose the files first."));
      const read = (f) => new Promise((resolve, reject) => { const r = new FileReader(); r.onload = () => resolve(r.result); r.onerror = () => reject(new Error(t("Cannot read {file}", { file: f.name }))); r.readAsDataURL(f); });
      const data = await Promise.all(list.map(read));
      const r = await api("POST", `/api/studies/${study.id}/add-series`, { description: description.value, files: data });
      toast(t(r.created === 1 ? "Series added ({n} file)." : "Series added ({n} files).", { n: r.created }));
      archiveChanged();
    });
}

/** The "..." menu of a study: change, move, anonymize, add a series, delete. */
function studyManageMenu(study) {
  const o = opts();
  return actionMenu("more", "Manage", "Change, move, anonymize or delete this study", [
    o.modify ? ["Change patient / study details...", () => openModifyDialog("studies", study)] : null,
    o.modify ? ["Move to another patient...", () => openMoveStudyDialog(study)] : null,
    o.anonymize ? ["Anonymize...", () => openAnonymizeDialog("studies", study)] : null,
    o.add_series ? ["Add a series (PDF, pictures)...", () => openAddSeriesDialog(study)] : null,
    o.delete ? ["Delete...", () => deleteStudies([study]), true] : null,
  ]);
}

/** The "..." menu of a series. */
function seriesManageMenu(series, study) {
  const o = opts();
  return actionMenu("more", "Manage", "Change, move, anonymize or delete this series", [
    o.modify ? ["Change the series details...", () => openModifyDialog("series", series)] : null,
    o.modify && study ? ["Move to another study...", () => openMoveSeriesDialog(series, study)] : null,
    o.anonymize ? ["Anonymize...", () => openAnonymizeDialog("series", series)] : null,
    o.delete ? ["Delete...", () => formDialog("Delete this series?", "Its images are removed from the archive for good.", null, "Delete the series", async () => {
      await api("DELETE", `/api/series/${series.id}`);
      toast("Series deleted.");
      archiveChanged();
    }, true), true] : null,
  ]);
}

/* ---- Query / retrieve ---- */

function buildRetrieveView() {
  const results = h("section", { class: "panel" }, h("div", { class: "empty" }, "Choose a system and search: its studies appear here, and you can bring them into this archive."));
  const source = h("select", { name: "source", "aria-label": "System" }, h("option", { value: "" }, "Loading..."));
  api("GET", "/api/retrieve/sources").then((list) => {
    source.replaceChildren(...(list.length ? list.map((n) => h("option", { value: n.id },
      [n.description || n.id, n.kind === "dicomweb" ? " (DICOMweb)" : n.aet ? ` ${t("({aet} at {address})", { aet: n.aet, address: `${n.host}:${n.port}` })}` : ""].join(""))) : [h("option", { value: "" }, "No other system is set up in Orthanc")]));
  }).catch(toastError);
  const box = (name, label, type = "text") => h("label", null, label, h("input", { name, type }));
  const form = h("form", {
    class: "panel filters",
    onsubmit: async (event) => {
      event.preventDefault();
      const body = { source: source.value };
      form.querySelectorAll("input").forEach((i) => { if (i.value.trim()) body[i.name] = i.value.trim(); });
      results.replaceChildren(h("div", { class: "loading" }, "Asking the other system..."));
      try {
        const r = await api("POST", "/api/retrieve/query", body);
        results.replaceChildren(r.answers.length ? renderAnswers(r.answers, body.source) : h("div", { class: "empty" }, "Nothing found there."));
      } catch (e) { results.replaceChildren(h("div", { class: "empty" }, e.message)); }
    },
  },
    h("div", { class: "filter-grid" },
      h("label", null, "System", source),
      box("patient_name", "Patient name"), box("patient_id", "Patient ID"), box("accession", "Accession number"),
      box("description", "Study description"), box("modality", "Modality (CT, MR...)"),
      box("date_from", "Study date from", "date"), box("date_to", "Study date to", "date")),
    h("div", { class: "filter-actions" }, h("button", { class: "btn primary", type: "submit" }, "Search there")));

  function renderAnswers(answers, sourceId) {
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Patient", "Patient ID", "Study date", "Description", "Modality", "Images", ""].map((t) => h("th", null, t)))),
      h("tbody", null, answers.map((a) => {
        const cell = h("td", null);
        const label = [formatName(a.patient_name), a.study_date, a.description].join(" | ");
        if (a.local_id) {
          cell.append(h("span", { class: "chip status-on" }, "Already here"), " ",
            h("button", { class: "btn small", type: "button", onclick: () => openSearchWith("studies", { study_uid: a.study_uid }) }, "Open"));
        } else {
          cell.append(h("button", {
            class: "btn small primary", type: "button",
            onclick: async (event) => {
              event.currentTarget.disabled = true;
              try {
                await api("POST", "/api/retrieve/retrieve", { source: sourceId, study_uid: a.study_uid, label });
                toast("Retrieving. Follow it on the Transfers page; the study then appears in the lists.", "info", 7000);
              } catch (e) { toastError(e); event.currentTarget.disabled = false; }
            },
          }, "Retrieve"));
        }
        return h("tr", null,
          h("td", null, formatName(a.patient_name)), h("td", { class: "mono" }, a.patient_id || dash()),
          h("td", { class: "nowrap" }, formatDicomDate(a.study_date) || dash()), h("td", null, a.description || dash()),
          h("td", null, a.modalities.length ? a.modalities.map(chip) : dash()), h("td", { class: "num" }, a.images || dash()), cell);
      }))));
  }

  return h("div", { class: "view" }, h("div", { class: "page-head" }, h("h1", null, "Retrieve from another system")), form, results);
}

/* ---- Worklists ---- */

function buildWorklistsView() {
  const container = h("section", { class: "panel" }, h("div", { class: "loading" }, "Loading..."));
  async function load() {
    try {
      const list = await api("GET", "/api/worklists");
      container.replaceChildren(list.length ? h("div", { class: "table-wrap" }, h("table", { class: "data" },
        h("thead", null, h("tr", null, ["Date", "Time", "Patient", "Patient ID", "Modality", "Description", "Station", "Accession", ""].map((t) => h("th", null, t)))),
        h("tbody", null, list.map((w) => h("tr", null,
          h("td", { class: "nowrap" }, formatDicomDate(w.date) || dash()), h("td", null, w.time ? `${w.time.slice(0, 2)}:${w.time.slice(2, 4)}` : dash()),
          h("td", null, formatName(w.patient_name)), h("td", { class: "mono" }, w.patient_id), h("td", null, chip(w.modality)),
          h("td", null, w.description || w.requested_procedure || dash()), h("td", { class: "mono" }, w.station_aet || dash()), h("td", { class: "mono" }, w.accession || dash()),
          h("td", null, h("div", { class: "actions" },
            actionButton("button", "edit", "Edit", "Change this worklist", { class: "btn small", type: "button", onclick: () => openWorklistDialog(w, load) }),
            actionButton("button", "revoke", "Delete", "Delete this worklist", {
              class: "btn small danger", type: "button",
              onclick: () => formDialog("Delete this worklist?", `${formatName(w.patient_name)}, ${w.modality} ${formatDicomDate(w.date)}`, null, "Delete", async () => {
                await api("DELETE", `/api/worklists/${encodeURIComponent(w.id)}`); toast("Worklist deleted."); load();
              }, true),
            })))))))) : h("div", { class: "empty" }, "No worklist. Add the examinations the scanners should see."));
    } catch (e) { container.replaceChildren(h("div", { class: "empty" }, e.message)); }
  }
  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Worklists"),
      h("div", { class: "head-actions" },
        actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load }),
        h("button", { class: "btn primary", type: "button", onclick: () => openWorklistDialog(null, load) }, "Add a worklist"))),
    h("p", { class: "muted" }, "The examinations the scanners find when they ask the archive for their worklist (DICOM Modality Worklist). Needs Orthanc's worklists plugin with its REST API."),
    container);
}

function openWorklistDialog(w, reload) {
  const fields = [["patient_name", "Patient name (DOE^JANE)"], ["patient_id", "Patient ID"], ["birth_date", "Birth date", "date"], ["sex", "Sex (M, F, O)"],
    ["modality", "Modality (CT, MR, US...)"], ["date", "Date", "date"], ["time", "Time", "time"], ["description", "Procedure step description"],
    ["requested_procedure", "Requested procedure"], ["station_aet", "Station AE title"], ["physician", "Performing physician"], ["accession", "Accession number"]];
  const toInput = (key, type, value) => type === "date" && /^\d{8}$/.test(value || "") ? `${value.slice(0, 4)}-${value.slice(4, 6)}-${value.slice(6)}`
    : type === "time" && /^\d{4}/.test(value || "") ? `${value.slice(0, 2)}:${value.slice(2, 4)}` : value || "";
  const inputs = fields.map(([key, label, type]) => h("label", null, label, h("input", { name: key, type: type || "text", value: toInput(key, type, w && w[key]) })));
  formDialog(w ? "Change the worklist" : "Add a worklist", null, h("div", { class: "settings-grid" }, inputs), w ? "Save" : "Add", async () => {
    const body = {};
    for (const label of inputs) {
      const input = label.querySelector("input");
      body[input.name] = input.type === "date" || input.type === "time" ? input.value.replace(/[-:]/g, "") : input.value.trim();
    }
    body.modality = (body.modality || "").toUpperCase();
    body.sex = (body.sex || "").toUpperCase();
    await api(w ? "PUT" : "POST", w ? `/api/worklists/${encodeURIComponent(w.id)}` : "/api/worklists", body);
    toast(w ? "Worklist changed." : "Worklist added.");
    reload();
  });
}

/* ---- Configuration > Jobs and System ---- */

/**
 * "Orthanc log" at the bottom of the System page: the end of one of Orthanc's log files (read by the
 * portal from the folder set in Settings > Orthanc: Orthanc's API cannot send it), with the file,
 * the number of lines, the levels shown and a search box. Lines are coloured by level (E, W, I, T).
 */
function buildOrthancLogPanel() {
  const panel = h("section", { class: "panel pad", id: "orthanc-log" }, h("h2", null, "Orthanc log"), h("div", { class: "loading" }, "Loading..."));
  const fileSelect = h("select", { "aria-label": "Log file" });
  const lineCount = h("select", { "aria-label": "Lines" }, [200, 500, 2000, 5000].map((n) => h("option", { value: n, selected: n === 500 }, t("last {n} lines", { n }))));
  const level = h("select", { "aria-label": "Levels" },
    h("option", { value: "" }, "every level"), h("option", { value: "EW" }, "warnings and errors"), h("option", { value: "E" }, "errors only"));
  const search = h("input", { type: "search", placeholder: "Search the lines", "aria-label": "Search the lines" });
  const info = h("p", { class: "muted" });
  const box = h("pre", { class: "raw-box log-box", tabindex: "0", "aria-label": "Orthanc log" });
  const download = h("a", { class: "btn small", href: "#" }, "Download the file");
  let lines = [];

  /** The level of an Orthanc log line: its first letter (E1118 10:15:00.123456 ...). */
  const levelOf = (line) => (/^[EWIT]\d{4} /.test(line) ? line[0] : "");

  function draw() {
    const q = search.value.trim().toLowerCase();
    const shown = lines.filter((l) => (!level.value || level.value.includes(levelOf(l))) && (!q || l.toLowerCase().includes(q)));
    box.replaceChildren(...shown.map((l) => h("span", { class: `log-line log-${levelOf(l) || "x"}` }, l, "\n")));
    if (!shown.length) box.textContent = t(lines.length ? "No line matches." : "The file is empty.");
    box.scrollTop = box.scrollHeight; // the newest lines
  }

  async function load(file = fileSelect.value) {
    const params = new URLSearchParams({ lines: lineCount.value });
    if (file) params.set("file", file);
    let r;
    try { r = await api("GET", `/api/admin/system/log?${params}`); } catch (e) {
      panel.replaceChildren(h("h2", null, "Orthanc log"), h("p", { class: "form-error" }, e.message));
      return;
    }
    if (!r.configured) {
      panel.replaceChildren(h("h2", null, "Orthanc log"),
        h("p", { class: "muted" }, "Orthanc's API can change the log level but cannot send the log itself. Enter the folder where Orthanc writes its log files in Settings > Orthanc, and it appears here."),
        h("button", { class: "btn small", type: "button", onclick: () => openSettings("orthanc") }, "Open Settings > Orthanc"));
      return;
    }
    if (!r.file) {
      panel.replaceChildren(h("h2", null, "Orthanc log"), h("p", { class: "muted" }, t("No log file in {folder}. Orthanc writes them there only when it is started with --logdir (or --logfile).", { folder: r.folder })));
      return;
    }
    fileSelect.replaceChildren(...r.files.map((f) => h("option", { value: f.name, selected: f.name === r.file },
      `${f.name} (${formatBytes(f.size)}, ${formatTimestamp(f.modified)})`)));
    lines = r.lines;
    info.textContent = t(r.truncated
      ? "{n} lines (the end of the file) of {file}, {size}, last written {date}. Every read is recorded in the audit log: the log can contain patient names and IDs."
      : "{n} lines of {file}, {size}, last written {date}. Every read is recorded in the audit log: the log can contain patient names and IDs.",
    { n: r.lines.length, file: r.file, size: formatBytes(r.size), date: formatTimestamp(r.modified) });
    download.href = `/api/admin/system/log/download?file=${encodeURIComponent(r.file)}`;
    if (!panel.contains(box)) {
      panel.replaceChildren(h("h2", null, "Orthanc log"),
        h("div", { class: "actions log-controls" }, fileSelect, lineCount, level, search,
          h("button", { class: "btn small", type: "button", onclick: () => load() }, "Refresh"), download),
        info, box);
    }
    draw();
  }

  fileSelect.addEventListener("change", () => load());
  lineCount.addEventListener("change", () => load());
  level.addEventListener("change", draw);
  search.addEventListener("input", draw);
  load("");
  return panel;
}

function buildJobsView() {
  const container = h("section", { class: "panel" }, h("div", { class: "loading" }, "Loading..."));
  async function load() {
    try {
      const jobs = await api("GET", "/api/admin/jobs");
      const act = (j, action, label) => h("button", {
        class: "btn small", type: "button",
        onclick: async () => { try { await api("POST", `/api/admin/jobs/${j.id}/${action}`); toast(`${lookup(label)}: ${j.type}`); load(); } catch (e) { toastError(e); } },
      }, label);
      container.replaceChildren(jobs.length ? h("div", { class: "table-wrap" }, h("table", { class: "data" },
        h("thead", null, h("tr", null, ["Created", "Type", "State", "Progress", "Run time", "Details", ""].map((t) => h("th", null, t)))),
        h("tbody", null, jobs.map((j) => h("tr", null,
          h("td", { class: "nowrap" }, orthancTime(j.created)), h("td", null, j.type), h("td", null, h("span", { class: `chip state-${String(j.state).toLowerCase()}` }, j.state)),
          h("td", { class: "num" }, `${j.progress ?? 0}%`), h("td", null, j.runtime != null ? formatDuration(j.runtime) : dash()),
          h("td", { class: "job-details" }, j.error && j.state === "Failure" ? j.error : jobSummary(j.content)),
          h("td", null, h("div", { class: "actions" },
            ["Pending", "Running"].includes(j.state) ? act(j, "pause", "Pause") : null,
            j.state === "Paused" ? act(j, "resume", "Resume") : null,
            ["Pending", "Running", "Paused", "Retry"].includes(j.state) ? act(j, "cancel", "Cancel") : null,
            j.state === "Failure" ? act(j, "resubmit", "Retry") : null))))))) : h("div", { class: "empty" }, "No job: Orthanc has nothing running and remembers no recent job."));
    } catch (e) { container.replaceChildren(h("div", { class: "empty" }, e.message)); }
  }
  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "Jobs"), actionButton("button", "refresh", "Refresh", "Refresh the list", { class: "btn", type: "button", onclick: load })),
    h("p", { class: "muted" }, "Everything Orthanc does in the background: sending, retrieving, modifying, anonymizing, archives... Orthanc keeps the recent ones only."),
    container);
}

/** Orthanc's "20261118T101500.123" as a local date and time. */
function orthancTime(text) {
  const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})/.exec(text || "");
  return m ? `${formatDicomDate(m[1] + m[2] + m[3])} ${m[4]}:${m[5]}:${m[6]}` : text || "";
}

/** A few words about what a job works on. */
function jobSummary(content) {
  if (!content || typeof content !== "object") return "";
  const parts = [];
  if (content.RemoteAet) parts.push(t("to {target}", { target: content.RemoteAet }));
  if (content.Peer) parts.push(t("to {target}", { target: [].concat(content.Peer).join(", ") }));
  if (content.InstancesCount != null) parts.push(imagesCount(content.InstancesCount));
  if (content.FailedInstancesCount) parts.push(t("{n} failed", { n: content.FailedInstancesCount }));
  if (content.Description) parts.push(content.Description);
  return parts.join(", ");
}

function buildSystemView() {
  const container = h("div", null, h("section", { class: "panel" }, h("div", { class: "loading" }, "Loading...")));
  async function load() {
    let info;
    try { info = await api("GET", "/api/admin/system"); } catch (e) { container.replaceChildren(h("section", { class: "panel empty" }, e.message)); return; }
    const s = info.system || {};
    const st = info.statistics || {};
    const row = (label, value) => (value === null || value === undefined || value === "" ? null : [h("dt", null, label), h("dd", null, String(value))]);
    const mb = (v) => (v == null ? null : formatBytes(Number(v) * 1024 * 1024));
    const level = h("select", { "aria-label": "Log level" }, ["default", "verbose", "trace"].map((l) => h("option", { value: l, selected: l === info.log_level }, l)));
    level.addEventListener("change", async () => {
      try { await api("PUT", "/api/admin/system/log-level", { level: level.value }); toast(t("Orthanc's log level is now {level}.", { level: level.value })); } catch (e) { toastError(e); }
    });
    const metrics = h("pre", { class: "raw-box", hidden: true });
    const panels = [
      h("section", { class: "panel pad" }, h("h2", null, "Orthanc"),
        h("dl", { class: "share-details" },
          row("Name", s.name), row("Version", s.version), row("REST API version", s.api_version), row("Database version", s.database_version),
          row("Database", s.database_backend ? "plugin" : "SQLite (built in)"), row("DICOM AE title and port", s.dicom_aet ? `${s.dicom_aet} : ${s.dicom_port}` : null),
          row("HTTP port", s.http_port), row("Overwrite instances", s.overwrite_instances), row("Ingest transcoding", s.ingest_transcoding),
          row("Maximum storage", s.maximum_storage_size ? `${s.maximum_storage_size} MB` : "no limit"),
          row("Maximum patients", s.maximum_patient_count || "no limit"), row("When full", s.maximum_storage_mode)),
        h("label", null, "Log level (until Orthanc restarts)", level)),
      h("section", { class: "panel pad" }, h("h2", null, "Storage and counts"),
        h("dl", { class: "share-details" },
          row("Patients", st.CountPatients), row("Studies", st.CountStudies), row("Series", st.CountSeries), row("Images", st.CountInstances),
          row("Disk used", st.TotalDiskSizeMB != null ? mb(st.TotalDiskSizeMB) : st.TotalDiskSize != null ? formatBytes(Number(st.TotalDiskSize)) : null),
          row("Uncompressed size", st.TotalUncompressedSizeMB != null ? mb(st.TotalUncompressedSizeMB) : null))),
      h("section", { class: "panel pad" }, h("h2", null, t("Plugins ({n})", { n: (info.plugins || []).length })),
        h("div", { class: "table-wrap" }, h("table", { class: "data" },
          h("thead", null, h("tr", null, h("th", null, "Plugin"), h("th", null, "Version"), h("th", null, "Description"))),
          h("tbody", null, (info.plugins || []).map((p) => h("tr", null, h("td", { class: "mono" }, p.name), h("td", null, p.version || dash()), h("td", null, p.description || dash()))))))),
      h("section", { class: "panel pad" }, h("h2", null, "Metrics"),
        h("p", { class: "muted" }, "Orthanc's counters in the Prometheus format (they need \"MetricsEnabled\" in Orthanc's configuration)."),
        h("button", {
          class: "btn small", type: "button",
          onclick: async () => {
            try { const r = await fetch("/api/admin/system/metrics", { credentials: "same-origin" }); metrics.textContent = r.ok ? await r.text() : translateMessage((await r.json()).error); } catch (e) { metrics.textContent = e.message; }
            metrics.hidden = false;
          },
        }, "Show the metrics"), metrics),
    ];
    if (info.raw_api) {
      const path = h("input", { value: "/system", "aria-label": "Address", class: "mono" });
      const out = h("pre", { class: "raw-box" });
      const read = async () => {
        out.textContent = "...";
        try {
          const r = await fetch(`/api/admin/orthanc?path=${encodeURIComponent(path.value.trim())}`, { credentials: "same-origin" });
          const text = await r.text();
          let shown = text;
          try { shown = JSON.stringify(JSON.parse(text), null, 2); } catch { /* text */ }
          out.textContent = r.ok ? shown : (translateMessage(JSON.parse(text).error) || text);
        } catch (e) { out.textContent = e.message; }
      };
      path.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); read(); } });
      panels.push(h("section", { class: "panel pad" }, h("h2", null, "Orthanc's REST API (read only)"),
        h("p", { class: "muted" }, "Read any address of Orthanc's API, for example /patients, /tools/labels or /studies/<id>. Only GET requests, never files; every read is in the audit log."),
        h("div", { class: "actions" }, path, h("button", { class: "btn small", type: "button", onclick: read }, "Read")), out));
    }
    panels.push(buildOrthancLogPanel());
    container.replaceChildren(...panels);
  }
  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "System"), actionButton("button", "refresh", "Refresh", "Refresh", { class: "btn", type: "button", onclick: load })),
    container);
}

/* ====================================================================
   9e. AUTO-FORWARD, BACKUPS, STATISTICS AND MY ACCOUNT
   ==================================================================== */

/**
 * Editor of the auto-forward rules (Settings > Auto-forward): one row per rule with its conditions
 * and its destination. `destinations` = the places studies can be sent to. `get()` = the rules.
 */
function buildForwardRulesEditor(rules, destinations) {
  const clone = (list) => list.map((r) => ({ ...r, modalities: (r.modalities || []).join(", "), labels: (r.labels || []).join(", ") }));
  let rows = clone(rules);
  const list = h("div", { class: "forward-rules" });
  const destinationSelect = (row) => {
    const known = destinations.some((d) => d.id === row.target);
    return h("select", { "aria-label": "Destination", onchange: (e) => { row.target = e.target.value; } },
      h("option", { value: "", selected: !row.target }, "Do not send it (only notify)"),
      !known && row.target ? h("option", { value: row.target, selected: true }, t("{name} (not found in Orthanc)", { name: row.target })) : null,
      destinations.map((d) => h("option", { value: d.id, selected: d.id === row.target }, d.description ? `${d.description}: ${d.id}` : d.id)));
  };
  const text = (row, key, placeholder, label, maxlength = 64) => h("input", {
    value: row[key] || "", placeholder, maxlength, "aria-label": label, oninput: (e) => { row[key] = e.target.value; },
  });
  function draw() {
    list.replaceChildren(...(rows.length ? rows.map((row, index) => h("div", { class: "forward-rule" },
      h("div", { class: "forward-rule-head" },
        h("label", { class: "check" }, h("input", { type: "checkbox", checked: row.enabled !== false, onchange: (e) => { row.enabled = e.target.checked; } }), "On"),
        text(row, "name", "Name, e.g. CT to reporting", "Rule name", 60),
        h("button", { class: "btn small danger", type: "button", onclick: () => { rows.splice(index, 1); draw(); } }, "Remove")),
      h("div", { class: "forward-rule-grid" },
        h("label", null, "Modalities (any of)", text(row, "modalities", "CT, MR (empty = any)", "Modalities", 200)),
        h("label", null, "Sent by (calling AE title)", text(row, "calling_aet", "SCANNER1 or SCAN* (empty = any)", "Calling AE title", 16)),
        h("label", null, "Institution contains", text(row, "institution", "(empty = any)", "Institution")),
        h("label", null, "Description contains", text(row, "description", "(empty = any)", "Description")),
        h("label", null, "Has a label (any of)", text(row, "labels", "urgent, teaching (empty = any)", "Labels", 400)),
        h("label", null, "Send to", destinationSelect(row)),
        h("label", null, "Notify by email", text(row, "notify", "a@example.org, b@example.org (empty = nobody)", "Notify by email", 1000)))))
      : [h("p", { class: "muted" }, "No rule yet.")]));
  }
  draw();
  return {
    node: h("div", { class: "wide" }, list,
      h("div", { class: "filter-actions" },
        h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ name: "", enabled: true, modalities: "", calling_aet: "", institution: "", description: "", labels: "", target: "", notify: "" }); draw(); } }, "Add rule")),
      h("p", { class: "hint" }, "A rule sends the study on, emails the addresses given, or both. The email goes out once per study, through the mail server of Settings > Email, and never names the patient: it gives the modalities, institution, description, date and accession number, and a link to the study when the portal's public address is set (Settings > Network & sharing). Labels are those the study has when it is complete: a label put on later does not count.")),
    get: () => rows.map((r) => ({
      name: (r.name || "").trim(), enabled: r.enabled !== false, target: r.target || "",
      modalities: String(r.modalities || "").split(/[\s,;]+/).map((c) => c.toUpperCase()).filter(Boolean),
      calling_aet: (r.calling_aet || "").trim(), institution: (r.institution || "").trim(), description: (r.description || "").trim(),
      labels: String(r.labels || "").split(/[\s,;]+/).filter(Boolean), notify: (r.notify || "").trim(),
    })),
  };
}

/** What the rules did lately (Settings > Auto-forward). */
function forwardStatus(status) {
  if (!status) return null;
  const lines = [];
  if (status.last_check) lines.push(t("Last look at Orthanc's new studies: {date}.", { date: formatTimestamp(status.last_check) }));
  lines.push(t("Studies sent by the rules since the portal started: {n}.", { n: status.forwarded || 0 }));
  if (status.notified) lines.push(t("Emails sent by the rules since the portal started: {n}.", { n: status.notified }));
  if (status.last_forward) lines.push(t("Last one: {date}, rule \"{rule}\".", { date: formatTimestamp(status.last_forward.ts), rule: status.last_forward.rule }));
  return h("div", { class: "setting wide" },
    h("ul", { class: "listener-status" }, lines.map((l) => h("li", null, l))),
    status.last_error ? h("p", { class: "form-error" }, translateMessage(status.last_error)) : null);
}

/** The backups in the folder, "Back up now" and "Download a copy" (Settings > Backups). */
function buildBackupsPanel() {
  if (!opts().backups) {
    return h("div", { class: "setting wide" }, h("p", { class: "muted" }, "Backups are switched off: tick the box above and save to use them."));
  }
  const listNode = h("div", null, h("div", { class: "loading" }, "Loading..."));
  async function load() {
    try {
      const r = await api("GET", "/api/admin/backups");
      listNode.replaceChildren(!r.dir
        ? h("p", { class: "muted" }, "No backup folder is set: there are no automatic backups. You can still download a copy.")
        : r.backups.length
          ? h("div", null,
            h("ul", { class: "listener-status" }, r.backups.map((b) => h("li", null, h("span", { class: "mono" }, b.name), ` · ${formatBytes(b.size)} · ${formatTimestamp(b.at)}`))),
            r.next ? h("p", { class: "muted" }, t("Next automatic backup: about {date}.", { date: formatTimestamp(r.next) })) : null)
          : h("p", { class: "muted" }, t("No backup in {dir} yet: the first one is made within ten minutes (or press Back up now).", { dir: r.dir })));
    } catch (e) { listNode.replaceChildren(h("p", { class: "form-error" }, e.message)); }
  }
  const now = h("button", { class: "btn", type: "button" }, "Back up now");
  now.addEventListener("click", async () => {
    now.disabled = true;
    try { const r = await api("POST", "/api/admin/backups"); toast(t("Backup written: {path}", { path: r.written })); load(); } catch (e) { toastError(e); }
    now.disabled = false;
  });
  load();
  return h("div", { class: "setting wide" },
    h("span", { class: "field-label" }, "Backups in the folder (uses the saved settings: save first)"), listNode,
    h("div", { class: "filter-actions" }, now,
      h("a", { class: "btn", href: "/api/admin/backup/download" }, "Download a copy now")),
    h("p", { class: "hint" }, "A backup holds the password hashes and the Orthanc and mail server passwords: keep it as safe as the server itself."));
}

/**
 * A small bar chart of one series over the months (one colour: it shows how much, not what).
 * Each bar names its month and value when the mouse rests on it; the table below the charts has
 * every number.
 */
function monthBars(title, months, values) {
  const max = Math.max(1, ...values);
  const width = 360, height = 120, gap = 4, base = height - 18;
  const barWidth = (width - gap * (months.length - 1)) / months.length;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("class", "month-bars");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", `${lookup(title)}: ${months.map((m, i) => `${m} ${values[i]}`).join(", ")}`);
  const line = document.createElementNS(SVG_NS, "line");
  for (const [k, v] of Object.entries({ x1: 0, x2: width, y1: base + 0.5, y2: base + 0.5, class: "axis" })) line.setAttribute(k, v);
  svg.append(line);
  months.forEach((month, i) => {
    const x = i * (barWidth + gap);
    const barHeight = values[i] ? Math.max(2, (values[i] / max) * (base - 14)) : 0;
    const hit = document.createElementNS(SVG_NS, "rect"); // a target taller than the bar
    for (const [k, v] of Object.entries({ x, y: 0, width: barWidth, height: base, class: "hit has-tip", "data-tip": `${month}: ${values[i]}`, tabindex: "0" })) hit.setAttribute(k, v);
    svg.append(hit);
    if (barHeight) {
      const bar = document.createElementNS(SVG_NS, "rect");
      for (const [k, v] of Object.entries({ x, y: base - barHeight, width: barWidth, height: barHeight, rx: 2, class: "bar" })) bar.setAttribute(k, v);
      svg.append(bar);
    }
    if (i % 3 === 2 || i === months.length - 1) {
      const label = document.createElementNS(SVG_NS, "text");
      for (const [k, v] of Object.entries({ x: x + barWidth / 2, y: height - 4, "text-anchor": "middle", class: "tick" })) label.setAttribute(k, v);
      label.textContent = month.slice(2);
      svg.append(label);
    }
  });
  const total = values.reduce((a, b) => a + b, 0);
  return h("figure", { class: "chart-card" },
    h("figcaption", null, h("b", null, title), h("span", { class: "muted" }, ` ${t("{n} in 12 months", { n: total.toLocaleString(i18n.locale) })}`)),
    svg);
}

/** Configuration > Statistics. */
function buildStatsView() {
  const container = h("div", { class: "view" }, h("div", { class: "panel loading" }, "Counting..."));
  async function load(refresh = false) {
    try {
      const d = await api("GET", `/api/admin/stats${refresh ? "?refresh=true" : ""}`);
      container.replaceChildren(...render(d));
    } catch (e) {
      container.replaceChildren(h("div", { class: "panel empty" }, e.message));
      toastError(e);
    }
  }
  function render(d) {
    const n = (v) => (typeof v === "number" ? v.toLocaleString(i18n.locale) : "?");
    const tile = (label, value) => h("div", { class: "stat-tile" }, h("b", null, value), h("span", null, label));
    const disk = typeof d.storage.disk_mb === "number" ? formatBytes(d.storage.disk_mb * 1048576) : "?";
    const tiles = h("section", { class: "stat-tiles" },
      tile("studies", n(d.storage.studies)), tile("patients", n(d.storage.patients)), tile("images", n(d.storage.instances)),
      tile("on disk", disk), tile("active share links", n(d.links.active_shares)), tile("active upload links", n(d.links.active_upload_links)));
    const months = d.months;
    const charts = h("section", { class: "chart-grid" },
      monthBars("Studies (by study date)", months, d.studies.total),
      monthBars("Share links created", months, d.links.shares_created),
      monthBars("Share links opened", months, d.links.shares_opened),
      monthBars("Studies received through upload links", months, d.links.studies_received),
      monthBars("Transfers to other systems", months, d.activity.transfers),
      monthBars("Sign-ins", months, d.activity.sign_ins));
    // The table: every number, by month (the modalities are the buttons of the search pages).
    const rows = [
      ["Studies, all modalities", d.studies.total],
      ...d.studies.by_modality.map((m) => [t("Studies with {m}", { m: m.label }), m.counts]),
      ["Share links created", d.links.shares_created], ["Share links opened", d.links.shares_opened],
      ["Share link downloads", d.links.shares_downloaded], ["Upload links created", d.links.upload_links_created],
      ["Studies received through upload links", d.links.studies_received], ["Study downloads (staff)", d.activity.downloads],
      ["Patient CD / USB exports", d.activity.cd_exports], ["Transfers", d.activity.transfers], ["Sign-ins", d.activity.sign_ins],
    ];
    const table = h("section", { class: "panel" }, h("h2", null, "Month by month"),
      h("div", { class: "table-wrap" }, h("table", { class: "data stats-table" },
        h("thead", null, h("tr", null, h("th", null, ""), months.map((m) => h("th", { class: "num" }, m)))),
        h("tbody", null, rows.map(([label, values]) => h("tr", null, h("th", { scope: "row" }, label), values.map((v) => h("td", { class: "num" }, n(v)))))))),
      !d.studies.counted ? h("p", { class: "form-error" }, "Orthanc could not count some months: their numbers show 0.") : null,
      h("p", { class: "muted" }, d.audit_retention_days
        ? t("Months are calendar months (UTC). The activity comes from the audit log, which keeps {n} days (Security & roles).", { n: d.audit_retention_days })
        : t("Months are calendar months (UTC). The activity comes from the audit log.")));
    const top = (title, list, unit) => h("section", { class: "panel" }, h("h2", null, title),
      list.length ? h("ol", { class: "top-list" }, list.map((x) => h("li", null, h("b", null, x.user), ` ${t(unit, { n: n(x.count) })}`)))
        : h("p", { class: "muted" }, "Nobody yet."));
    const tops = h("div", { class: "stat-tops" },
      top("Who uploads most (90 days)", d.top.uploaders, "{n} files"),
      top("Who shares most (90 days)", d.top.sharers, "{n} links"),
      top("Who downloads most (90 days)", d.top.downloaders, "{n} studies"));
    return [tiles, charts, table, tops, h("p", { class: "muted" }, t("Counted {date}.", { date: formatTimestamp(d.generated_at) }))];
  }
  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h2", null, "Statistics"),
      actionButton("button", "refresh", "Count again", "Count again (otherwise the numbers are kept for 10 minutes)", { class: "btn", type: "button", onclick: () => load(true) })),
    container);
}

/** Asks for the password again before a change of the two-step sign-in; resolves to it, or null. */
function askPassword(title, intro) {
  return new Promise((resolve) => {
    let value = null;
    const input = h("input", { type: "password", autocomplete: "current-password", required: true });
    const dialog = formDialog(title, intro, [h("label", null, "Your password", input)], "Continue", async () => { value = input.value; });
    dialog.addEventListener("close", () => resolve(value));
    input.focus();
  });
}

/** My account: email, password and two-step sign-in. */
function buildAccountView() {
  const view = h("div", { class: "view account" });
  async function refreshMe() {
    try { state.user = await api("GET", "/api/me"); } catch { /* keep what we have */ }
    view.replaceChildren(...draw().flat().filter(Boolean));
  }

  function draw() {
    const user = state.user;
    const mode = user.features.two_factor || "off";
    // ---- email
    const email = h("input", { type: "email", value: user.email || "", placeholder: "name@example.org", autocomplete: "email" });
    const emailPanel = h("section", { class: "panel" }, h("h2", null, "Email address"),
      h("p", { class: "muted" }, "Where the portal writes to you: when a link you created is opened or downloaded, or when files arrive through your upload link (if you ask for it on the link)."),
      h("form", { class: "inline-field", onsubmit: async (e) => {
        e.preventDefault();
        try { await api("PUT", "/api/account/email", { email: email.value }); toast("Saved."); refreshMe(); } catch (err) { toastError(err); }
      } }, email, h("button", { class: "btn", type: "submit" }, "Save")));

    // ---- password
    const current = h("input", { type: "password", autocomplete: "current-password", required: true });
    const fresh = h("input", { type: "password", autocomplete: "new-password", required: true, minlength: 10 });
    const again = h("input", { type: "password", autocomplete: "new-password", required: true, minlength: 10 });
    const passwordPanel = h("section", { class: "panel" }, h("h2", null, "Password"),
      h("form", { class: "settings-grid", onsubmit: async (e) => {
        e.preventDefault();
        if (fresh.value !== again.value) { toast("The two new passwords are not the same.", "error"); return; }
        try {
          await api("POST", "/api/account/password", { current: current.value, new: fresh.value });
          toast("Password changed. Your other sessions were signed out.");
          current.value = fresh.value = again.value = "";
        } catch (err) { toastError(err); }
      } },
      h("label", null, "Current password", current), h("label", null, "New password (at least 10 characters)", fresh),
      h("label", null, "New password again", again), h("div", null, h("button", { class: "btn", type: "submit" }, "Change password"))));

    // ---- two-step sign-in
    let twoFactorPanel = null;
    if (mode !== "off") {
      const body = h("div");
      if (user.two_factor) {
        body.append(
          h("p", null, h("span", { class: "chip status-on" }, "On"), " ", "Every sign-in asks for a code from your authenticator app."),
          h("p", { class: "muted" }, t("Recovery codes left: {n}.", { n: user.recovery_codes_left })),
          h("div", { class: "filter-actions" },
            h("button", { class: "btn", type: "button", onclick: async () => {
              const password = await askPassword("New recovery codes", "The codes you have now stop working.");
              if (!password) return;
              try { const r = await api("POST", "/api/account/two-factor/recovery", { password }); showRecoveryCodes(r.recovery_codes); refreshMe(); } catch (e) { toastError(e); }
            } }, "New recovery codes"),
            mode === "optional" ? h("button", { class: "btn danger", type: "button", onclick: async () => {
              const password = await askPassword("Switch off two-step sign-in", "From then on the password alone signs you in.");
              if (!password) return;
              try { await api("POST", "/api/account/two-factor/disable", { password }); toast("Two-step sign-in is off."); refreshMe(); } catch (e) { toastError(e); }
            } }, "Switch it off") : h("p", { class: "muted" }, "It is required in this portal: it cannot be switched off.")));
      } else {
        const start = h("button", { class: "btn primary", type: "button" }, "Set it up");
        start.addEventListener("click", async () => {
          const password = await askPassword("Set up two-step sign-in", "Your password first, to be sure it is you.");
          if (!password) return;
          let material;
          try { material = await api("POST", "/api/account/two-factor/setup", { password }); } catch (e) { toastError(e); return; }
          const code = h("input", { name: "code", inputmode: "numeric", autocomplete: "one-time-code", required: true, maxlength: 10, placeholder: "123456" });
          body.replaceChildren(twoFactorSetupBlock(material), h("form", { class: "inline-field", onsubmit: async (e) => {
            e.preventDefault();
            try { const r = await api("POST", "/api/account/two-factor/enable", { code: code.value }); showRecoveryCodes(r.recovery_codes); refreshMe(); } catch (err) { toastError(err); code.select(); }
          } }, code, h("button", { class: "btn primary", type: "submit" }, "Confirm")));
          code.focus();
        });
        body.append(h("p", null, h("span", { class: "chip status-off" }, "Off"), " ", "Protect your account with a code from your phone, asked after the password at every sign-in."), start);
      }
      twoFactorPanel = h("section", { class: "panel" }, h("h2", null, "Two-step sign-in"), body);
    }

    return [
      h("div", { class: "page-head" }, h("h1", null, "My account")),
      h("section", { class: "panel" }, h("h2", null, user.username),
        h("p", null, user.permissions.length ? user.permissions.map((p) => h("span", { class: "chip perm" }, p)) : h("span", { class: "muted" }, "no access")),
        h("p", { class: "muted" }, t("Last sign-in: {date}.", { date: formatTimestamp(user.last_login) }))),
      opts().account_self_service !== false ? [emailPanel, passwordPanel] : null, twoFactorPanel];
  }
  view.replaceChildren(...draw().flat().filter(Boolean));
  return view;
}

/* ====================================================================
   10. START-UP
   ==================================================================== */

/** Asks the server whether a session already exists (cookie), then draws the page. */
(async function boot() {
  readSearchFromAddress(); // a link with filters (e.g. /?patient_id=P-001) opens the Search page, after signing in if needed
  try { state.brand = await api("GET", "/api/public"); } catch { /* keep the default identity */ }
  await loadLanguage(chosenLanguage());
  applyBrand();
  try { state.user = await api("GET", "/api/me"); } catch { state.user = null; }
  renderApp();
})();
