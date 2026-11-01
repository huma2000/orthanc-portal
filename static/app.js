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
    5. Shell: login, navigation, sign out
    6. Studies and Series search    (shared builder, modality picker, two tables)
   6b. Preview thumbnails           (lazy loading, small in-memory cache)
    7. Viewers (Weasis / OHIF), downloads and sending to DICOM nodes
    8. Upload                       (queue, parallel XHR uploads, drag & drop)
    9. Transfers, Users (admin) and Audit log
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
 * Quick modality filters shown as toggle buttons. `codes` are the DICOM modality
 * codes sent to the server; several can be selected at once (CT *or* US...).
 * "RX" is X-ray, which DICOM splits into CR (computed) and DX (digital).
 */
const MODALITY_CHIPS = [
  { label: "CT", codes: ["CT"], title: "Computed tomography" },
  { label: "MR", codes: ["MR"], title: "Magnetic resonance" },
  { label: "US", codes: ["US"], title: "Ultrasound" },
  { label: "RX", codes: ["CR", "DX"], title: "X-ray (CR + DX)" },
  { label: "MG", codes: ["MG"], title: "Mammography" },
  { label: "XA", codes: ["XA"], title: "X-ray angiography" },
  { label: "RF", codes: ["RF"], title: "Fluoroscopy" },
  { label: "NM", codes: ["NM"], title: "Nuclear medicine" },
  { label: "PT", codes: ["PT"], title: "PET" },
];

/** Global UI state. `user` is null while signed out. */
const state = {
  user: null,     // { id, username, permissions: ["read", ...], scope: "full"|"remote", features: { ohif, thumbnails } }
  view: null,     // id of the visible tab ("studies", "series", "upload", "users", "audit")
  notice: "",     // message shown on the login form (e.g. "Your session expired")
  // Identity of this installation (Settings > Branding). Filled from /api/public.
  brand: { name: "Orthanc Portal", title: "Orthanc Portal", show_name: true, login_notice: "", logo: null,
           favicon: { url: "/favicon.svg", type: "image/svg+xml", custom: false } },
  orthancProblem: "",     // administrators: why the Orthanc connection is not healthy ("" = fine)
  orthancCheckedAt: 0,    // when it was last checked (ms), to test at most once a minute
};

const root = document.getElementById("app");

/** Does the signed-in user own this permission? (UI convenience only: the server enforces it.) */
const can = (permission) => !!state.user && state.user.permissions.includes(permission);

/** Is the OHIF browser viewer available? (decided by the server from Orthanc's plugins) */
const hasOhif = () => !!(state.user && state.user.features && state.user.features.ohif);

/**
 * May Weasis be used? Only on the intranet. Weasis saves the images on the computer, so over
 * the internet the browser viewer (OHIF) is the only way to look at them (the server also
 * refuses Weasis links and tokens there; hiding the button is just the polite part).
 */
const hasWeasis = () => !!(state.user && state.user.features && state.user.features.weasis);

/** Is this session limited to viewing (internet access through Cloudflare)? */
const isRemote = () => !!state.user && state.user.scope === "remote";

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
const chip = (modality) => h("span", { class: "chip", "data-m": modality }, modality || "?");

/** Grey placeholder for empty values. */
const dash = () => h("span", { class: "muted" }, "-");

/* ====================================================================
   5. SHELL: LOGIN, NAVIGATION, SIGN OUT
   ==================================================================== */

/** The tabs. `needs` is the permission required to see the tab. */
const VIEWS = [
  { id: "studies", label: "Studies", needs: "read", build: () => buildSearchView(STUDY_SEARCH) },
  { id: "series", label: "Series", needs: "read", build: () => buildSearchView(SERIES_SEARCH) },
  { id: "upload", label: "Upload", needs: "upload", build: buildUploadView },
  { id: "transfers", label: "Transfers", needs: "send", build: buildTransfersView },
  { id: "users", label: "Users", needs: "admin", build: buildUsersView },
  { id: "nodes", label: "DICOM nodes", needs: "admin", build: buildNodesView },
  { id: "settings", label: "Settings", needs: "admin", build: buildSettingsView },
  { id: "audit", label: "Audit log", needs: "admin", build: buildAuditView },
];

/** Draws the whole page: login form when signed out, otherwise top bar + current tab. */
function renderApp() {
  root.replaceChildren();
  if (!state.user) {
    root.append(buildLogin());
    return;
  }

  // Only the tabs this user may use.
  const views = VIEWS.filter((v) => can(v.needs));
  if (!views.some((v) => v.id === state.view)) state.view = views.length ? views[0].id : null;
  const current = views.find((v) => v.id === state.view);

  const nav = h("nav", { class: "nav", "aria-label": "Main" },
    views.map((v) => h("button", {
      type: "button",
      "aria-current": v.id === state.view ? "page" : null,
      onclick: () => { state.view = v.id; renderApp(); },
    }, v.label)));

  const topbar = h("header", { class: "topbar" },
    h("div", { class: "brand brand-slot", "data-size": "24" }, ...brandContent(24, false)),
    nav,
    h("div", { class: "userbox" },
      isRemote()
        ? h("span", { class: "badge-remote", title: "Connected through the internet: you can search and view studies, nothing else." }, "Remote access, view only")
        : null,
      h("span", null, state.user.username),
      h("button", { class: "btn small", type: "button", onclick: signOut }, "Sign out")));

  const page = h("main", { class: "page" },
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

/** Applies the identity everywhere it shows: browser tab title, website icon, logo and name. */
function applyBrand() {
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
  renderApp();
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

/** Downloads one thumbnail. Resolves to an object URL, or "" when there is no preview. */
async function loadThumb(url) {
  if (thumbImages.has(url)) return thumbImages.get(url);
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
  if (thumbImages.has(box.thumbUrl)) { paintThumb(box, thumbImages.get(box.thumbUrl)); return; }
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
 */
function thumb(kind, id, modality, small = false) {
  const box = h("div", { class: small ? "thumb small loading" : "thumb loading", "aria-hidden": "true" },
    h("span", { class: "thumb-label" }, modality || ""));
  box.thumbUrl = `/api/${kind}/${id}/thumbnail`;
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
    { name: "accession_number", label: "Accession number" },
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
 * Modality filter: a row of toggle buttons (CT, MR, US, RX...) plus a small box for
 * any other DICOM code. Several can be active at once ("CT or US"). It exposes
 * `.value` (comma-separated codes) like an <input>, so the generic search code can
 * read, store and clear it. `onChange` is called when a button is toggled.
 */
function buildModalityPicker(initial, onChange) {
  const known = new Set(MODALITY_CHIPS.flatMap((chip) => chip.codes));
  const selected = new Set();          // active codes that have a button
  const other = h("input", { class: "mod-other", placeholder: "Other codes: SR, ES...", "aria-label": "Other modality codes" });
  const buttons = MODALITY_CHIPS.map((chip) => ({
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

  let hint = null;
  if (can("visualize")) {
    if (!hasWeasis()) {
      // Over the internet: images are shown in the browser only, never downloaded to the computer.
      hint = h("p", { class: "muted" }, hasOhif()
        ? "Images open in the browser viewer in a new tab. They are not downloaded to this computer."
        : "Images cannot be viewed over the internet: the browser viewer (OHIF) is not available on this server.");
    } else {
      hint = h("p", { class: "muted" }, "Weasis needs to be ",
        h("a", { href: "https://weasis.org", target: "_blank", rel: "noopener" }, "installed"),
        " on this computer",
        hasOhif() ? "; the browser viewer (OHIF) works without installing anything." : ".");
    }
  }

  run(0); // show the most recent studies right away
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, cfg.title), hint), form, results);
}

/** Results table of the "Studies" tab. Each study row can expand to show its series. */
function renderStudiesTable(studies) {
  const headers = ["", ...(showThumbs() ? ["Preview"] : []), "Patient", "Patient ID", "Study date", "Description", "Modality", "Series / images", "Accession", "Actions"];
  return h("div", { class: "table-wrap" },
    h("table", { class: "data" },
      h("thead", null, h("tr", null, headers.map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
      h("tbody", null, studies.map(studyRows))));
}

/** Two table rows per study: the study itself and a hidden row that will hold its series. */
function studyRows(study) {
  const seriesCell = h("td", { colspan: showThumbs() ? 10 : 9 });
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
    h("td", null, formatDicomDate(study.study_date) || dash()),
    h("td", null, study.study_description || dash()),
    h("td", null, study.modalities.length ? study.modalities.map(chip) : dash()),
    h("td", null, `${study.series_count ?? "?"} series`, imageInfo ? h("div", { class: "sub" }, imageInfo) : null),
    h("td", { class: "mono" }, study.accession_number || dash()),
    h("td", null, h("div", { class: "actions" }, studyActions(study))));

  return [mainRow, seriesRow];
}

/** Buttons shown for a study, according to the user's permissions. */
function studyActions(study) {
  return [
    can("visualize") ? viewMenu(study) : null,
    can("send") ? h("button", { class: "btn small", type: "button", onclick: () => openSendDialog(study) }, "Send to...") : null,
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
          h("td", null, study ? formatDicomDate(study.study_date) : dash()),
          h("td", null, (study && study.study_description) || dash()),
          h("td", null, s.series_number || dash()),
          h("td", null, chip(s.modality)),
          h("td", null, s.description || dash()),
          h("td", null, s.body_part || dash()),
          h("td", { class: "num" }, s.instances_count),
          h("td", null, h("div", { class: "actions" }, [
            can("visualize") && study ? viewMenu(study) : null,
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

/** Plain "Open in Weasis" button (used when OHIF is not available). */
function weasisButton(studyUid, label) {
  const button = h("button", { class: "btn small", type: "button" }, label);
  button.addEventListener("click", () => openInWeasis(studyUid, button));
  return button;
}

/** Address of the OHIF web viewer for a study (served by Orthanc's OHIF plugin through the portal). */
const ohifUrl = (studyUid) => `/ohif/viewer?StudyInstanceUIDs=${encodeURIComponent(studyUid)}`;

/**
 * "View" control of a study. With OHIF available it is a small menu offering both
 * viewers; otherwise it is just the Weasis button. This works for remote
 * (internet) users too, because viewing is the one thing they are allowed to do.
 */
function viewMenu(study) {
  if (!hasWeasis()) {
    // Over the internet: one button, the browser viewer. (No viewer at all when the OHIF
    // plugin is missing; the search page explains why.)
    return hasOhif()
      ? h("a", { class: "btn small view-link", href: ohifUrl(study.study_instance_uid), target: "_blank", rel: "noopener", title: "Opens in a new tab" }, "View")
      : null;
  }
  if (!hasOhif()) return weasisButton(study.study_instance_uid, "Open in Weasis");

  const weasisItem = h("button", { class: "menu-item", type: "button" },
    "Weasis", h("small", null, "Desktop viewer, must be installed"));
  weasisItem.addEventListener("click", (event) => {
    closeMenu(event);
    openInWeasis(study.study_instance_uid, weasisItem);
  });
  const ohifItem = h("a", { class: "menu-item", href: ohifUrl(study.study_instance_uid), target: "_blank", rel: "noopener", onclick: closeMenu },
    "Browser viewer (OHIF)", h("small", null, "Opens in a new tab, nothing to install"));

  return h("details", { class: "menu" },
    h("summary", { class: "btn small" }, "View"),
    h("div", { class: "menu-list" }, weasisItem, ohifItem));
}

/**
 * "Download" drop-down for a study or series (`kind` = "studies" | "series").
 * Plain links: the browser itself downloads the ZIP, streamed from the server,
 * so even very large studies work and the browser shows the progress.
 */
function downloadMenu(kind, id) {
  return h("details", { class: "menu" },
    h("summary", { class: "btn small" }, "Download"),
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
    h("option", { value: n.id }, n.aet ? `${n.id}  (${n.aet} at ${n.host}:${n.port})` : n.id)));
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
};

/** Starts a fresh batch (counters to zero) when nothing is running. */
function resetUploadStats() {
  Object.assign(upload, { total: 0, uploaded: 0, duplicates: 0, failed: 0, errors: [], finished: false });
  upload.studyIds = new Set();
}

/** Adds files to the queue and starts sending. */
function enqueueFiles(fileList) {
  // Skip hidden/system files and DICOMDIR (an index, not an image).
  const files = Array.from(fileList).filter((f) => !f.name.startsWith(".") && f.name.toUpperCase() !== "DICOMDIR");
  if (!files.length) { toast("No uploadable files found.", "error"); return; }
  if (upload.next >= upload.pending.length && !upload.inFlight.size) resetUploadStats();
  // A loop, not push(...files): spreading a very large array (a whole archive folder)
  // exceeds the call-stack limit.
  for (const file of files) upload.pending.push(file);
  upload.total += files.length;
  upload.finished = false;
  pump();
}

/** Keeps up to UPLOAD_CONCURRENCY requests running. */
function pump() {
  while (upload.inFlight.size < UPLOAD_CONCURRENCY && upload.next < upload.pending.length) {
    const file = upload.pending[upload.next++];
    const item = { file, loaded: 0 };
    upload.inFlight.set(file, item);
    sendFile(item).then(() => {
      upload.inFlight.delete(file);
      pump();
    });
  }
  if (upload.next >= upload.pending.length && upload.pending.length) { upload.pending = []; upload.next = 0; } // free the list
  scheduleUploadUi();
  if (upload.next >= upload.pending.length && !upload.inFlight.size && upload.total > 0 && !upload.finished) {
    upload.finished = true;
    updateUploadUi();
    toast(`Upload finished: ${upload.uploaded} new, ${upload.duplicates} already stored, ${upload.failed} failed.`,
      upload.failed ? "error" : "info", 8000);
  }
}

function failFile(file, message) {
  upload.failed++;
  if (upload.errors.length < 200) upload.errors.push({ name: file.webkitRelativePath || file.name, message });
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
      if (xhr.status >= 200 && xhr.status < 300) {
        if (data && data.Status === "AlreadyStored") upload.duplicates++; else upload.uploaded++;
        if (data && data.ParentStudy) upload.studyIds.add(data.ParentStudy);
      } else {
        failFile(item.file, (data && data.error) || `HTTP ${xhr.status}`);
        if (xhr.status === 401) { upload.pending = []; upload.next = 0; sessionExpired(); } // stop: we are signed out
      }
      resolve();
    };
    xhr.onerror = () => { failFile(item.file, "Network error"); resolve(); };
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

/** Redraws counters, progress bar, active files and error list (if the tab is visible). */
function updateUploadUi() {
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
  return files;
}

/** The "Upload" tab. */
function buildUploadView() {
  const onPicked = (event) => { enqueueFiles(event.target.files); event.target.value = ""; };
  const fileInput = h("input", { type: "file", multiple: true, hidden: true, onchange: onPicked });
  const folderInput = h("input", { type: "file", multiple: true, webkitdirectory: true, hidden: true, onchange: onPicked });

  const dropzone = h("div", { class: "dropzone" },
    h("h2", null, "Drop DICOM files or a whole study folder here"),
    h("p", { class: "muted" }, "DICOM files (.dcm or without extension) and ZIP archives are accepted. Each file is sent separately."),
    h("div", { class: "buttons" },
      h("button", { class: "btn primary", type: "button", onclick: () => fileInput.click() }, "Choose files"),
      h("button", { class: "btn", type: "button", onclick: () => folderInput.click() }, "Choose folder")),
    fileInput, folderInput);

  dropzone.addEventListener("dragover", (e) => { e.preventDefault(); dropzone.classList.add("over"); });
  dropzone.addEventListener("dragleave", () => dropzone.classList.remove("over"));
  dropzone.addEventListener("drop", async (e) => {
    e.preventDefault();
    dropzone.classList.remove("over");
    enqueueFiles(await filesFromDrop(e.dataTransfer));
  });

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
    onclick: () => { if (can("read")) { state.view = "studies"; renderApp(); } },
  }, "Browse studies");

  const progressPanel = h("section", { class: "panel" }, barWrap, stats, active, errors, h("div", { class: "upload-errors" }, viewButton));
  const rootNode = h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Upload")), dropzone, progressPanel);

  upload.ui = { root: rootNode, bar, barWrap, stats, statDone, statNew, statDup, statFail, statStudies, active, errors, viewButton };
  updateUploadUi();
  return rootNode;
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

  async function load() {
    try {
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

  function renderTransfers(rows) {
    return h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", null, h("tr", null, ["Started", "Study", "Destination", "By", "State"].map((t) => h("th", null, t)))),
      h("tbody", null, rows.map((r) => {
        const fill = h("span");
        fill.style.width = `${r.progress}%`;
        return h("tr", null,
          h("td", { class: "nowrap" }, formatTimestamp(r.ts)),
          h("td", null, r.label.replaceAll(" | ", " - ")),
          h("td", null, h("span", { class: "chip" }, r.target)),
          h("td", null, r.username),
          h("td", null,
            h("span", { class: `chip state-${r.state.toLowerCase()}` }, r.state),
            r.state === "Running" || r.state === "Pending" ? h("div", { class: "progress small" }, fill) : null,
            r.error ? h("div", { class: "error-line" }, r.error) : null));
      }))));
  }

  load();
  return h("div", { class: "view" },
    h("div", { class: "page-head" }, h("h1", null, "Transfers"), h("button", { class: "btn", type: "button", onclick: load }, "Refresh")),
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

  function renderUsersTable() {
    return h("div", { class: "table-wrap" },
      h("table", { class: "data" },
        h("thead", null, h("tr", null, ["User", "Access", "Status", "Last sign-in", "Actions"].map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
        h("tbody", null, meta.users.map((u) => h("tr", null,
          h("td", null, h("span", { class: "patient" }, u.username), u.id === state.user.id ? h("span", { class: "sub" }, " (you)") : null),
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
      h("h1", null, "Users"),
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

  const preset = h("select", {
    onchange: (event) => {
      const chosen = meta.presets.find((p) => p.key === event.target.value);
      if (chosen) for (const key of Object.keys(checks)) checks[key].checked = chosen.permissions.includes(key);
    },
  }, h("option", { value: "" }, "Custom"), meta.presets.map((p) => h("option", { value: p.key }, p.label)));

  const active = h("input", { type: "checkbox", checked: user ? user.active : true });

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      const permissions = Object.keys(checks).filter((key) => checks[key].checked);
      try {
        if (isNew) {
          await api("POST", "/api/admin/users", { username: username.value.trim(), password: password.value, permissions });
        } else {
          const body = { permissions, active: active.checked };
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
    h("label", null, "Start from a role", preset),
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
    h("div", { class: "page-head" }, h("h1", null, "Audit log"), h("button", { class: "btn", type: "button", onclick: load }, "Refresh")),
    container);
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
      h("button", { class: "btn small", type: "button", onclick: () => { state.view = "settings"; renderApp(); } }, "Open settings"));
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
    { key: "orthanc_username", label: "Orthanc user name", hint: "Leave empty if Orthanc has no authentication." },
  ],
  network: [
    { key: "bind_addr", label: "Intranet address (all features)", placeholder: "0.0.0.0:8080", hint: "IP:PORT. 0.0.0.0 = every network card." },
    { key: "remote_enabled", label: "Enable the remote (internet) listener", type: "checkbox", wide: true },
    { key: "remote_bind_addr", label: "Remote address (view only)", placeholder: "0.0.0.0:8081", hint: "Point the Cloudflare Tunnel here. Only searching and viewing work through it." },
    { key: "remote_block_dicom_files", label: "Internet viewers cannot download whole DICOM files (recommended)", type: "checkbox", wide: true, hint: "Over the internet only the browser viewer (OHIF) is available, because Weasis would download the images. This also refuses requests for entire .dcm files through the viewer's address: the viewer only needs metadata and pixel frames. Switch it off only if some special object type fails to display remotely." },
    { key: "public_base_url", label: "Public address for Weasis (optional)", placeholder: "https://pacs.example.com", hint: "Empty = automatic: intranet users get the intranet address, internet users the Cloudflare one." },
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
    { key: "ohif", label: "Browser viewer (OHIF)", type: "select", options: [["auto", "Automatic (when Orthanc has the plugin)"], ["on", "Always offer it"], ["off", "Never offer it"]] },
    { key: "max_upload_mb", label: "Largest uploaded file (MB)", type: "number", min: 1, max: 102400, hint: "Applies to each file; a study is uploaded as many files." },
    { key: "thumbnails_enabled", label: "Show preview thumbnails next to studies and series", type: "checkbox", wide: true, hint: "Only users with the visualize permission see them, on the intranet and remotely. Each one makes Orthanc decode one image." },
    { key: "thumbnail_cache_mb", label: "Thumbnail cache (MB)", type: "number", min: 0, max: 1024, hint: "Memory used to remember generated thumbnails. 0 = no cache." },
  ],
};

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
  const clone = (list) => list.map((r) => ({ key: r.key, label: r.label, permissions: [...r.permissions] }));
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
    h("button", { class: "btn small danger", type: "button", onclick: () => { rows.splice(index, 1); draw(); } }, "Remove"))));
  draw();

  const node = h("div", { class: "wide" }, list,
    h("div", { class: "filter-actions" },
      h("button", { class: "btn small", type: "button", onclick: () => { rows.push({ key: "", label: "", permissions: ["read", "visualize"] }); draw(); } }, "Add role"),
      h("button", { class: "btn small", type: "button", onclick: () => { rows = clone(defaults); draw(); } }, "Restore default roles")));

  return { node, get: () => rows.map((r) => ({ key: r.key.trim(), label: r.label.trim(), permissions: r.permissions })) };
}

/** The "Settings" tab: the complete configuration of the portal. */
function buildSettingsView() {
  const view = h("div", { class: "view" });
  let doc = null; // { settings, listeners, defaults, permissions } from the server

  async function load() {
    view.replaceChildren(h("div", { class: "page-head" }, h("h1", null, "Settings")), h("div", { class: "panel loading" }, "Loading settings..."));
    try {
      doc = await api("GET", "/api/admin/settings");
      draw([]);
    } catch (e) {
      view.replaceChildren(h("div", { class: "page-head" }, h("h1", null, "Settings")), h("div", { class: "panel empty" }, e.message));
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
        input = h("input", { type: def.type || "text", value: value ?? "", min: def.min, max: def.max, placeholder: def.placeholder });
      }
      inputs[def.key] = input;
      const control = def.type === "checkbox" ? h("label", { class: "check" }, input, def.label) : h("label", null, def.label, input);
      return h("div", { class: def.wide ? "setting wide" : "setting" }, control, def.hint ? h("p", { class: "hint" }, def.hint) : null);
    };

    /** A titled card of the form. */
    const section = (title, intro, ...children) => h("section", { class: "panel settings-section" },
      h("h2", null, title), intro ? h("p", { class: "muted" }, intro) : null, h("div", { class: "settings-grid" }, children));

    // ---- Orthanc connection: password is write-only, plus a "Test connection" button
    const password = h("input", { type: "password", autocomplete: "new-password", placeholder: s.orthanc_password_set ? "A password is saved. Leave empty to keep it." : "No password saved" });
    const clearPassword = h("input", { type: "checkbox" });
    const testBox = h("div", { class: "test-result", hidden: true, role: "status" });
    const testButton = h("button", { class: "btn", type: "button" }, "Test connection");
    testButton.addEventListener("click", async () => {
      testButton.disabled = true;
      testBox.hidden = false;
      testBox.className = "test-result";
      testBox.textContent = "Testing...";
      try {
        const r = await api("POST", "/api/admin/orthanc/test", {
          url: inputs.orthanc_url.value, username: inputs.orthanc_username.value, password: password.value,
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
        save.disabled = true;
        // Collect the values of every control into the object the server expects.
        const body = {};
        for (const [key, input] of Object.entries(inputs)) {
          body[key] = input.type === "checkbox" ? input.checked : input.type === "number" ? Number(input.value) : input.value;
        }
        body.roles = rolesEditor.get();
        if (password.value) body.orthanc_password = password.value;
        if (clearPassword.checked) body.clear_orthanc_password = true;
        try {
          doc = await api("PUT", "/api/admin/settings", body);
          await refreshBrand();
          if (state.user.features) state.user.features.thumbnails = doc.settings.thumbnails_enabled;
          refreshOrthancStatus(true);
          toast("Settings saved.");
          draw(doc.messages || []);
        } catch (e) {
          toastError(e);
          save.disabled = false;
        }
      },
    },
      section("Branding", "Make the portal look like your practice. The window title, the portal name and the login notice are saved with the button below; the logo and the website icon are saved as soon as you choose a file.",
        SETTINGS_FIELDS.portal.map(field),
        buildBrandImagePicker("logo", "Logo", "PNG, JPEG, WebP, GIF or SVG, up to 1 MB. Shown in the top bar (about 32 px high, on a light plate so any logo is readable) and on the login page.", "image/png,image/jpeg,image/webp,image/gif,image/svg+xml"),
        buildBrandImagePicker("favicon", "Website icon", "PNG, ICO, SVG, JPEG, WebP or GIF, up to 256 KB. Square, at least 64 x 64 pixels. Shown in the browser tab and in bookmarks.", "image/png,image/x-icon,image/vnd.microsoft.icon,image/svg+xml,image/jpeg,image/webp,image/gif")),
      section("Orthanc connection", "The PACS this portal works with. Changes apply immediately, without restarting.",
        SETTINGS_FIELDS.orthanc.map(field),
        h("div", { class: "setting" },
          h("label", null, "Orthanc password", password),
          h("label", { class: "check" }, clearPassword, "Remove the saved password")),
        h("div", { class: "setting wide" }, testButton, testBox)),
      section("Network", "Two listeners: the intranet one (everything) and the remote one (view only, for the Cloudflare Tunnel). Changing an address re-opens the listener right away; if the new address cannot be opened, nothing is changed. Running in Docker? Leave the two addresses as they are: Docker publishes fixed ports.",
        SETTINGS_FIELDS.network.map(field), listenerInfo),
      section("Sessions and sign-in security", null, SETTINGS_FIELDS.security.map(field)),
      section("Viewers and uploads", null, SETTINGS_FIELDS.viewers.map(field)),
      section("Roles", "Shortcuts that pre-tick permissions when you create or edit a user. Users keep their own permissions: changing a role does not change existing users.", rolesEditor.node),
      h("div", { class: "settings-actions" }, save));

    view.replaceChildren(
      h("div", { class: "page-head" }, h("h1", null, "Settings")),
      notices.length ? h("div", { class: "notice", role: "status" }, notices) : null,
      form);
  }

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
      h("thead", null, h("tr", null, ["Name", "AE title", "Address", "Send", "Ping", "Actions"].map((t) => h("th", { class: t === "Actions" ? "num" : null }, t)))),
      h("tbody", null, nodes.map((n) => h("tr", null,
        h("td", null, h("span", { class: "patient" }, n.id)),
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
    h("div", { class: "page-head" }, h("h1", null, "DICOM nodes"),
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
  const allowStore = h("input", { type: "checkbox", checked: node ? node.allow_store : true });
  const allowEcho = h("input", { type: "checkbox", checked: node ? node.allow_echo : true });

  const dialog = h("dialog", null, h("form", {
    onsubmit: async (event) => {
      event.preventDefault();
      error.hidden = true;
      try {
        await api("PUT", `/api/admin/nodes/${encodeURIComponent(isNew ? name.value.trim() : node.id)}`, {
          aet: aet.value, host: host.value, port: Number(port.value),
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
