/*
  share.js - the page a guest sees when they open an invitation link (/s/<token>).

  Deliberately small and independent of app.js: this is the only script served on the
  internet-facing side. It never contains HTML from the server (every value is inserted
  as text), and it keeps no secret: the password is typed once and only the server's
  cookie proves the session afterwards.

  Screens
    unavailable  the link is unknown or was withdrawn
    expired      the link ran out of time
    locked       asks for the password
    open         the study: details, series, "View online", "Download", time and downloads left
*/
"use strict";

const root = document.getElementById("app");
/** The random part of the address: /s/<48 hex characters>. */
const TOKEN = (location.pathname.match(/^\/s\/([0-9a-f]{48})\/?$/i) || [])[1] || "";

const state = {
  brand: { name: "", show_name: true, logo: null },
  clockOffset: 0, // server time minus this computer's time (seconds): the server's clock rules
  expiresAt: 0,
  timer: null,
};

/* ---------------------------------------------------------------- translations */

/*
 * The same scheme as app.js (section 1b), smaller: English in the code, a dictionary
 * static/i18n/<code>.json for another language. The portal's language comes from /api/public; a
 * visitor may choose another one in the menu of the page (remembered in this browser).
 */
const LANGUAGE_KEY = "portal.language";
const i18n = { lang: "en", dict: null, messages: {}, patterns: [], locale: undefined };

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

/** Translates `text` and fills its {names}: t("{n} shared studies", { n: 2 }). */
function t(text, values) {
  const out = lookup(text);
  return values ? out.replace(/\{(\w+)\}/g, (all, name) => (name in values ? String(values[name]) : all)) : out;
}

/** A message of the server: an exact key, or a key whose {names} match any value. */
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

/** The visitor's choice (this browser), or the portal's language. */
function chosenLanguage(info) {
  const offered = ((info && info.languages) || []).map(([code]) => code);
  let mine = "";
  try { mine = localStorage.getItem(LANGUAGE_KEY) || ""; } catch { /* private window */ }
  if (offered.includes(mine)) return mine;
  return offered.includes(info && info.language) ? info.language : "en";
}

async function loadLanguage(lang) {
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

/* ---------------------------------------------------------------- tiny helpers */

/** Builds an element. Strings become text nodes, so nothing can inject HTML. Texts are translated. */
const TRANSLATED_ATTRIBUTES = new Set(["placeholder", "title", "aria-label", "alt"]);
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (["value", "disabled", "hidden", "selected"].includes(key)) el[key] = value;
    else el.setAttribute(key, value === true ? "" : TRANSLATED_ATTRIBUTES.has(key) ? lookup(value) : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(typeof child === "string" ? lookup(child) : String(child)));
  }
  return el;
}

class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

/** JSON API call (always sends the marker header the server requires on POST). */
async function api(method, url, body) {
  const options = { method, credentials: "same-origin", headers: { "X-Requested-With": "portal" } };
  if (body !== undefined) {
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(url, options);
  } catch {
    throw new ApiError(t("Cannot reach the server. Check your internet connection."), 0);
  }
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* not JSON */ }
  if (!response.ok) throw new ApiError(translateMessage(data && data.error) || t("Request failed ({status})", { status: response.status }), response.status);
  return data;
}

/** How the practice writes dates and names (Settings > Appearance, from /api/public). */
const format = () => (state.brand && state.brand.format) || {};

/** "DOE^JOHN" -> "DOE, JOHN" (or "JOHN DOE", or as stored). */
function formatName(raw) {
  if (!raw) return t("(no name)");
  if (format().name === "dicom") return raw.replace(/\^+$/, "");
  const parts = raw.split("^").map((p) => p.trim()).filter(Boolean);
  if (parts.length < 2) return parts[0] || raw;
  return format().name === "first_last" ? `${parts.slice(1).join(" ")} ${parts[0]}` : `${parts[0]}, ${parts.slice(1).join(" ")}`;
}

/**
 * DICOM date 20240517 -> "17 May 2024" in the visitor's language, or the practice's own format
 * when it chose one other than the default.
 */
function formatDate(value) {
  if (!/^\d{8}$/.test(value || "")) return value || "";
  const [y, m, d] = [value.slice(0, 4), value.slice(4, 6), value.slice(6, 8)];
  const style = format().date;
  if (style === "dmy") return `${d}/${m}/${y}`;
  if (style === "mdy") return `${m}/${d}/${y}`;
  if (style === "dmy_dots") return `${d}.${m}.${y}`;
  const date = new Date(Number(value.slice(0, 4)), Number(value.slice(4, 6)) - 1, Number(value.slice(6, 8)));
  return date.toLocaleDateString(i18n.locale, { year: "numeric", month: "long", day: "numeric" });
}

/** Seconds -> "1 d 3 h 20 min" / "45 min" / "30 s". */
function formatDuration(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  const days = Math.floor(s / 86400), hours = Math.floor((s % 86400) / 3600), minutes = Math.floor((s % 3600) / 60);
  if (days > 0) return `${days} d ${hours} h ${minutes} min`;
  if (hours > 0) return `${hours} h ${minutes} min`;
  if (minutes > 0) return `${minutes} min`;
  return `${s} s`;
}

const nowSeconds = () => Date.now() / 1000 + state.clockOffset;

/* ---------------------------------------------------------------- page frame */

/** The last screen drawn, to draw it again in another language. */
let redraw = null;

/** The language menu of the page (none when the portal offers one language only). */
function languageMenu() {
  const list = state.languages || [];
  if (list.length < 2) return null;
  return h("select", {
    class: "language-select", "aria-label": "Language", title: "Language",
    onchange: async (event) => {
      try { localStorage.setItem(LANGUAGE_KEY, event.target.value); } catch { /* only for this page */ }
      await loadLanguage(event.target.value);
      if (redraw) redraw();
    },
  }, list.map(([code, name]) => h("option", { value: code, selected: code === i18n.lang, lang: code }, name)));
}

/** Header with the practice's logo / name, then the given content. */
function draw(...content) {
  const brand = state.brand;
  const head = h("header", { class: "share-head" },
    brand.logo ? h("img", { class: "brand-logo", src: brand.logo.url, alt: brand.show_name ? "" : brand.name }) : null,
    brand.show_name ? h("span", { class: "brand-name" }, brand.name) : null,
    languageMenu());
  root.replaceChildren(h("main", { class: "share-wrap" }, head, ...content));
}

/** A simple message card (link unavailable / expired / nothing to open). */
function message(title, text) {
  redraw = () => message(title, text);
  draw(h("section", { class: "share-card" }, h("h1", null, title), h("p", { class: "muted" }, text)));
}

/* ---------------------------------------------------------------- screens */

function showPasswordForm() {
  redraw = showPasswordForm;
  const error = h("div", { class: "form-error", role: "alert", hidden: true });
  const input = h("input", {
    id: "share-password", name: "password", type: "text", autocomplete: "off", autocapitalize: "characters",
    spellcheck: "false", placeholder: "XXXX-XXXX-XXXX", required: true, "aria-describedby": "share-help",
  });
  const button = h("button", { class: "btn primary big", type: "submit" }, "Open the study");

  const form = h("form", {
    class: "share-password",
    onsubmit: async (event) => {
      event.preventDefault();
      button.disabled = true;
      error.hidden = true;
      try {
        await api("POST", `/api/share/${TOKEN}/unlock`, { password: input.value });
        await showStudy();
      } catch (e) {
        if (e.status === 410) { return message("This link is no longer available", e.message); }
        error.textContent = e.message;
        error.hidden = false;
        button.disabled = false;
        input.select();
      }
    },
  }, error, h("label", { for: "share-password" }, "Password", input), button);

  draw(h("section", { class: "share-card" },
    h("h1", null, "A study has been shared with you"),
    h("p", { id: "share-help", class: "muted" }, "Enter the password you were given to open it. Capital letters, spaces and dashes do not matter."),
    form));
  input.focus();
}

/** Loads the study and draws it. */
async function showStudy() {
  let data;
  try {
    data = await api("GET", `/api/share/${TOKEN}/study`);
  } catch (e) {
    if (e.status === 401) return showPasswordForm(); // the session ended
    if (e.status === 410) return message("This link is no longer available", e.message);
    return message("The study cannot be shown right now", e.message);
  }
  state.clockOffset = data.now - Date.now() / 1000;
  state.expiresAt = data.expires_at;
  redraw = showStudy;

  const { downloads, viewer } = data;
  const studies = data.studies && data.studies.length ? data.studies : [{ patient: data.patient, study: data.study, series: data.series, index: 0, viewer_url: viewer.url }];
  if (studies.length > 1) return showStudies(data, studies);
  const { patient, study, series } = studies[0];
  const row = (label, value) => (value ? [h("dt", null, label), h("dd", null, value)] : []);
  const details = h("dl", { class: "share-details" },
    row("Patient", formatName(patient.name)),
    row("Patient ID", patient.id),
    row("Birth date", formatDate(patient.birth_date)),
    row("Sex", patient.sex),
    row("Study date", formatDate(study.date)),
    row("Description", study.description),
    row("Accession", study.accession),
    row("Modalities", study.modalities.length ? study.modalities.map((m) => h("span", { class: "chip", "data-m": m }, m)) : ""));

  const table = series.length
    ? h("div", { class: "table-wrap" }, h("table", { class: "data" },
        h("thead", null, h("tr", null, ["#", "Modality", "Description", "Body part", "Images"].map((t) => h("th", { class: t === "Images" ? "num" : null }, t)))),
        h("tbody", null, series.map((s) => h("tr", null,
          h("td", null, s.number || "-"), h("td", null, h("span", { class: "chip", "data-m": s.modality }, s.modality || "?")),
          h("td", null, s.description || "-"), h("td", null, s.body_part || "-"), h("td", { class: "num" }, s.images))))))
    : h("p", { class: "muted" }, "This study has no series.");

  // ---- the two actions
  const view = viewer.available
    ? h("a", { class: "btn primary big", href: viewer.url, target: "_blank", rel: "noopener" }, "View online")
    : h("button", { class: "btn big", type: "button", disabled: true, title: "The online viewer is not available on this server" }, "View online");
  let download;
  if (downloads.max === 0) {
    download = h("button", { class: "btn big", type: "button", disabled: true }, "Download not enabled");
  } else if (downloads.left === 0) {
    download = h("button", { class: "btn big", type: "button", disabled: true }, "No downloads left");
  } else {
    download = h("a", {
      class: "btn big", href: `/api/share/${TOKEN}/download`, download: "",
      // A download uses up one of the allowed ones as soon as it starts: refresh the counter shortly after.
      onclick: () => { setTimeout(refreshDownloads, 3000); setTimeout(refreshDownloads, 12000); },
    }, "Download (ZIP)");
  }

  // ---- time and downloads left
  const remaining = h("b", { id: "share-remaining" });
  const left = h("b", { id: "share-downloads" });
  const meta = h("div", { class: "share-meta", "aria-live": "polite" },
    h("div", null, "This link expires in ", remaining, h("span", { class: "muted" }, ` (${new Date(data.expires_at * 1000).toLocaleString(i18n.locale)})`)),
    h("div", null, "Downloads left: ", left));

  draw(h("section", { class: "share-card" },
    h("h1", null, "Shared study"), details, table,
    h("div", { class: "share-actions" }, view, download),
    meta,
    h("p", { class: "muted" }, "You can look at the study online until the link expires, even when no downloads are left."),
    h("button", { class: "btn small", type: "button", onclick: closeSession }, "Close this session")));
  paintCountdown(downloads);
  clearInterval(state.timer);
  state.timer = setInterval(() => paintCountdown(), 1000);
  state.downloads = downloads;
}

/** A link for several studies: each with its details, series, viewer and download; "view all" at the top. */
function showStudies(data, studies) {
  const { downloads, viewer } = data;
  const row = (label, value) => (value ? [h("dt", null, label), h("dd", null, value)] : []);
  const canDownload = downloads.max > 0 && downloads.left > 0;
  const blocks = studies.map((item) => {
    const { patient, study, series } = item;
    return h("section", { class: "share-study" },
      h("h2", null, `${formatDate(study.date)}  ${study.description || ""}`.trim()),
      h("dl", { class: "share-details" },
        row("Patient", formatName(patient.name)), row("Patient ID", patient.id), row("Accession", study.accession),
        row("Modalities", study.modalities.length ? study.modalities.map((m) => h("span", { class: "chip", "data-m": m }, m)) : ""),
        row("Series", t("{n} ({images} images)", { n: series.length, images: series.reduce((n, s) => n + (Number(s.images) || 0), 0) }))),
      h("div", { class: "share-actions" },
        viewer.available ? h("a", { class: "btn", href: item.viewer_url, target: "_blank", rel: "noopener" }, "View online") : null,
        canDownload ? h("a", {
          class: "btn", href: `/api/share/${TOKEN}/download?study=${item.index}`, download: "",
          onclick: () => { setTimeout(refreshDownloads, 3000); setTimeout(refreshDownloads, 12000); },
        }, "Download (ZIP)") : null));
  });
  const remaining = h("b", { id: "share-remaining" });
  const left = h("b", { id: "share-downloads" });
  draw(h("section", { class: "share-card" },
    h("h1", null, t("{n} shared studies", { n: studies.length })),
    h("div", { class: "share-actions" },
      viewer.available ? h("a", { class: "btn primary big", href: viewer.url, target: "_blank", rel: "noopener" }, "View them all online") : null),
    blocks,
    h("div", { class: "share-meta", "aria-live": "polite" },
      h("div", null, "This link expires in ", remaining, h("span", { class: "muted" }, ` (${new Date(data.expires_at * 1000).toLocaleString(i18n.locale)})`)),
      h("div", null, "Downloads left: ", left, h("span", { class: "muted" }, " (each study downloaded uses one)"))),
    h("p", { class: "muted" }, "You can look at the studies online until the link expires, even when no downloads are left."),
    h("button", { class: "btn small", type: "button", onclick: closeSession }, "Close this session")));
  paintCountdown(downloads);
  clearInterval(state.timer);
  state.timer = setInterval(() => paintCountdown(), 1000);
  state.downloads = downloads;
}

/** Updates the "expires in" and "downloads left" texts; the link ends when the time is up. */
function paintCountdown(downloads) {
  if (downloads) state.downloads = downloads;
  const seconds = state.expiresAt - nowSeconds();
  if (seconds <= 0) {
    clearInterval(state.timer);
    return message("This link has expired", "The time of this invitation is over. Ask the person who sent it for a new one.");
  }
  const remaining = document.getElementById("share-remaining");
  const left = document.getElementById("share-downloads");
  if (remaining) remaining.textContent = formatDuration(seconds);
  if (left && state.downloads) left.textContent = state.downloads.max === 0 ? t("none (online viewing only)") : t("{done} of {total}", { done: state.downloads.left, total: state.downloads.max });
}

/** Re-reads the downloads left (they change when a download starts). */
async function refreshDownloads() {
  try {
    const data = await api("GET", `/api/share/${TOKEN}/study`);
    if (data.downloads.left !== (state.downloads && state.downloads.left)) await showStudy();
  } catch { /* the next tick of the page will show what is wrong */ }
}

async function closeSession() {
  try { await api("POST", `/api/share/${TOKEN}/lock`); } catch { /* leaving anyway */ }
  location.reload();
}

/* ---------------------------------------------------------------- colours */

/**
 * The light/dark mode, accent and background chosen in the portal's Settings > Appearance
 * (a smaller copy of applyTheme in app.js: this page has no header or icon column).
 */
function applyTheme(theme) {
  const t = theme || {};
  const html = document.documentElement;
  const isColor = (v) => /^#[0-9a-f]{6}$/i.test(v || "");
  if (t.mode === "light" || t.mode === "dark") html.dataset.theme = t.mode;
  if (isColor(t.accent)) {
    const rgb = [1, 3, 5].map((i) => parseInt(t.accent.slice(i, i + 2), 16) / 255)
      .map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
    const light = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2] > 0.179;
    html.classList.add("custom-accent");
    html.style.setProperty("--accent", t.accent);
    html.style.setProperty("--accent-hover", t.accent);
    html.style.setProperty("--on-accent", light ? "#15212c" : "#ffffff");
  }
  if (isColor(t.background)) html.style.setProperty("--paper", t.background);
}

/* ---------------------------------------------------------------- start */

(async function start() {
  try {
    const info = await api("GET", "/api/public");
    state.brand = { name: info.name, show_name: info.show_name, logo: info.logo, format: info.format || {} };
    state.languages = info.languages || [];
    applyTheme(info.theme);
    await loadLanguage(chosenLanguage(info));
  } catch { /* the page still works without branding (in English) */ }
  if (!TOKEN) return message("Open your invitation link", "This address only serves studies that were shared with you. Use the link you were sent.");
  let status;
  try {
    status = (await api("GET", `/api/share/${TOKEN}/status`)).state;
  } catch (e) {
    return message("The page cannot be shown right now", e.message);
  }
  if (status === "open") return showStudy();
  if (status === "locked") return showPasswordForm();
  if (status === "expired") return message("This link has expired", "The time of this invitation is over. Ask the person who sent it for a new one.");
  return message("This link is not available", "The link is not valid or was withdrawn. Ask the person who sent it for a new one.");
})();
