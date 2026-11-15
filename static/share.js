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

/* ---------------------------------------------------------------- tiny helpers */

/** Builds an element. Strings become text nodes, so nothing can inject HTML. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") el.className = value;
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (["value", "disabled", "hidden"].includes(key)) el[key] = value;
    else el.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
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
    throw new ApiError("Cannot reach the server. Check your internet connection.", 0);
  }
  const text = await response.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* not JSON */ }
  if (!response.ok) throw new ApiError((data && data.error) || `Request failed (${response.status})`, response.status);
  return data;
}

/** "DOE^JOHN" -> "DOE, JOHN". */
function formatName(raw) {
  if (!raw) return "(no name)";
  const parts = raw.split("^").map((p) => p.trim()).filter(Boolean);
  return parts.length > 1 ? `${parts[0]}, ${parts.slice(1).join(" ")}` : parts[0] || raw;
}

/** DICOM date 20240517 -> "17 May 2024" in the visitor's language. */
function formatDate(value) {
  if (!/^\d{8}$/.test(value || "")) return value || "";
  const date = new Date(Number(value.slice(0, 4)), Number(value.slice(4, 6)) - 1, Number(value.slice(6, 8)));
  return date.toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
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

/** Header with the practice's logo / name, then the given content. */
function draw(...content) {
  const brand = state.brand;
  const head = h("header", { class: "share-head" },
    brand.logo ? h("img", { class: "brand-logo", src: brand.logo.url, alt: brand.show_name ? "" : brand.name }) : null,
    brand.show_name ? h("span", { class: "brand-name" }, brand.name) : null);
  root.replaceChildren(h("main", { class: "share-wrap" }, head, ...content));
}

/** A simple message card (link unavailable / expired / nothing to open). */
function message(title, text) {
  draw(h("section", { class: "share-card" }, h("h1", null, title), h("p", { class: "muted" }, text)));
}

/* ---------------------------------------------------------------- screens */

function showPasswordForm() {
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

  const { patient, study, series, downloads, viewer } = data;
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
    h("div", null, "This link expires in ", remaining, h("span", { class: "muted" }, ` (${new Date(data.expires_at * 1000).toLocaleString()})`)),
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
  if (left && state.downloads) left.textContent = state.downloads.max === 0 ? "none (online viewing only)" : `${state.downloads.left} of ${state.downloads.max}`;
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

/* ---------------------------------------------------------------- start */

(async function start() {
  try {
    const info = await api("GET", "/api/public");
    state.brand = { name: info.name, show_name: info.show_name, logo: info.logo };
  } catch { /* the page still works without branding */ }
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
