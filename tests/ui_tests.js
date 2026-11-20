// Browser-side tests: the portal's two web pages driven in jsdom against the live portal + fake Orthanc.
//   * the staff UI (static/app.js) on the intranet listener, and
//   * the guest page (static/share.js) that an invitation link opens on the internet listener.
const { JSDOM } = require("jsdom");
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const check = (name, ok, extra = "") => { results.push(ok); console.log(`${ok ? "PASS" : "FAIL"}  ${name} ${ok ? "" : extra}`); };
const APP = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const SHARE = fs.readFileSync(path.join(__dirname, "..", "static", "share.js"), "utf8");
const LOGS = process.env.MOCK_LOG_DIR || "/tmp/portal-tests";
const STAFF = "http://127.0.0.1:18080", GUEST = "http://127.0.0.1:18081";
const SID = "aaaaaaaa-bbbbbbbb-cccccccc-dddddddd-eeeeeeee"; // the study of the fake Orthanc
const STUDY_UID = "1.2.826.0.1.3680043.8.498.1";
const SHARE_SECTION = "Share links (invitations for people outside)";

/** Runs one SQL statement on the portal's database (used to move a link's expiry around). */
const sql = (statement) => execFileSync(process.env.PYTHON3 || "python3", ["-c", "import sqlite3,sys;c=sqlite3.connect(sys.argv[1],timeout=10);c.execute(sys.argv[2]);c.commit()", path.join(LOGS, "portal.db"), statement]);
const nowSeconds = () => Math.floor(Date.now() / 1000);
/** Waits (up to `ms`) until `fn()` is truthy; the pages load their data asynchronously. */
async function until(fn, ms = 6000) {
  const end = Date.now() + ms;
  while (Date.now() < end) { try { if (fn()) return true; } catch { /* not there yet */ } await sleep(100); }
  return false;
}

/**
 * Opens a page of the portal in jsdom: `pathname` on `base`, running `script` (app.js or share.js).
 * `window.fetch` keeps a tiny cookie jar, like a browser would.
 */
async function makeDom(base, pathname = "/", script = APP) {
  let cookie = "";
  const html = (await (await fetch(base + pathname)).text()).replace(/<script src[^>]*><\/script>/, "");
  const dom = new JSDOM(html, { url: base + pathname, runScripts: "outside-only", pretendToBeVisual: true });
  const { window } = dom;
  const errors = [];
  window.addEventListener("error", (e) => errors.push(e.message));
  window.fetch = async (url, opts = {}) => {
    const headers = { ...(opts.headers || {}) };
    if (cookie) headers.Cookie = cookie;
    const res = await fetch(base + url, { ...opts, headers, redirect: "manual" });
    const sc = res.headers.get("set-cookie");
    if (sc) cookie = sc.split(";")[0].endsWith("=") ? "" : sc.split(";")[0];
    return res;
  };
  window.URL.createObjectURL = () => "blob:x"; window.URL.revokeObjectURL = () => {};
  window.HTMLDialogElement.prototype.showModal = function () { this.setAttribute("open", ""); };
  window.HTMLDialogElement.prototype.close = function () { this.removeAttribute("open"); this.dispatchEvent(new window.Event("close")); };
  window.confirm = () => true;
  window.eval(script);
  const t = {
    window, errors, call: (method, url, body) => window.fetch(url, { method, headers: { "X-Requested-With": "portal", "Content-Type": "application/json" }, body: body && JSON.stringify(body) }),
    $: (s) => window.document.querySelector(s),
    $$: (s) => Array.from(window.document.querySelectorAll(s)),
    text: () => window.document.body.textContent,
    click: (el) => el.dispatchEvent(new window.MouseEvent("click", { bubbles: true, cancelable: true })), // cancelable: a test may preventDefault() a link (jsdom cannot navigate)
    submit: (el) => el.dispatchEvent(new window.Event("submit", { cancelable: true, bubbles: true })),
    async login(user, pass) {
      await sleep(500);
      t.$("input[name=username]").value = user; t.$("input[name=password]").value = pass;
      t.submit(t.$(".login-card form")); await sleep(1500);
    },
    // The icon column (pages), without the Log out button.
    tabs: () => t.$$(".side-btn").map((b) => b.dataset.tip).filter((n) => !n.startsWith("Log out")),
    // Opens a page by its icon. "Studies" and "Series" are the two tabs of the Search page;
    // "Settings" is the Configuration icon.
    tab: async (name) => {
      const icon = { Studies: "Search", Series: "Search", Settings: "Configuration" }[name] || name;
      t.click(t.$$(".side-btn").find((b) => b.dataset.tip === icon)); await sleep(1200);
      if (name === "Studies" || name === "Series") {
        const tab = t.$$(".subtabs [role=tab]").find((b) => b.textContent === name);
        if (tab.getAttribute("aria-selected") !== "true") { t.click(tab); await sleep(1200); }
      }
    },
    section: (title) => t.$$("section.settings-section").find((s) => s.querySelector("h2").textContent === title),
    labelInput: (scope, text) => Array.from(scope.querySelectorAll("label")).find((l) => l.textContent.startsWith(text)).querySelector("input,select,textarea"),
    // A button by its text, or an icon button by its name (data-name: "Weasis", "Share...", "Revoke"...).
    button: (scope, text) => Array.from(scope.querySelectorAll("button, a.btn, summary")).find((b) => b.textContent === text || b.dataset.name === text),
    // Everything a row offers, in words: its text plus the names of its icon buttons.
    words: (el) => `${el.textContent} ${Array.from(el.querySelectorAll("[data-name]")).map((b) => b.dataset.name).join(" ")}`,
  };
  return t;
}

/** A guest opens an invitation link: the page, then the password typed into its form. */
async function guestOpens(token, password) {
  const page = await makeDom(GUEST, `/s/${token}`, SHARE);
  await until(() => page.$(".share-card"));
  if (password && page.$("#share-password")) {
    page.$("#share-password").value = password;
    page.submit(page.$("form.share-password"));
    await until(() => !page.$("#share-password"), 5000);
    await sleep(300);
  }
  return page;
}

/** Creates a link as an administrator through the API (the dialog is tested separately). */
async function newLink(admin, body) {
  const out = await (await admin.call("POST", `/api/studies/${SID}/share`, body)).json();
  return { ...out, token: out.url.split("/s/")[1] };
}

(async () => {
  const all_extra = []; // more pages checked for JavaScript errors at the end
  console.log("--- intranet administrator: the staff portal");
  const admin = await makeDom(STAFF);
  await admin.login("admin", "SuperSecret123");
  // Lists that refresh themselves would add searches to the logs the checks read: off, except in their own part.
  await admin.call("PUT", "/api/admin/settings", { recent_auto_refresh: false, transfers_auto_refresh: false });
  await sleep(1200);
  check("all pages are in the icon column, including Shares and Transfers", admin.tabs().join() === "Home,Search,Shares,Uploads,Transfers,My account,Help,Configuration", admin.tabs().join());
  check("the home page opens first, with the recent studies", !!admin.$(".recent:not(.recently-viewed)") && /Recent studies/.test(admin.$(".recent:not(.recently-viewed)").textContent));
  check("there is a single view: no view switch", !admin.$$(".side-btn").some((b) => /view/i.test(b.dataset.tip)) && !admin.$(".nav"));

  console.log("--- Settings is organised in tabs (users, DICOM nodes and the audit log live there)");
  await admin.tab("Settings");
  await until(() => admin.$("form.settings-form"), 6000);
  const subtab = (name) => admin.$$(".subtabs [role=tab]").find((b) => b.textContent === name);
  const visibleSections = () => admin.$$("section.settings-section").filter((s) => !s.hidden).map((s) => s.querySelector("h2").textContent);
  check("Settings tabs", admin.$$(".subtabs [role=tab]").map((b) => b.textContent).join() === "General,Appearance,Search & labels,Features,Orthanc,Network & sharing,Security & roles,Auto-forward,Backups,Users,DICOM nodes,Audit log,Jobs,System,Statistics",
    admin.$$(".subtabs [role=tab]").map((b) => b.textContent).join());
  check("no stray 'null' text on the page (it used to appear under the title)",
    !admin.$$("main *").some((el) => Array.from(el.childNodes).some((n) => n.nodeType === 3 && n.textContent.trim() === "null")));
  check("General shows Branding and Viewers", visibleSections().join() === "Branding,Viewers and uploads", visibleSections().join());
  admin.click(subtab("Search & labels"));
  check("Search & labels shows the modality buttons, the lists and search options, and the labels", visibleSections().join() === "Modality buttons,Studies lists and search,Labels", visibleSections().join());
  admin.click(subtab("Orthanc"));
  check("Orthanc shows the connection only, with the Save button", visibleSections().join() === "Orthanc connection" && !admin.$("form.settings-form").closest("[hidden]"), visibleSections().join());
  admin.labelInput(admin.section("Orthanc connection"), "Orthanc address").value = "http://typed-not-saved:8042";
  admin.click(subtab("Users")); await until(() => admin.$$("main h2").some((e) => e.textContent === "Users") && admin.$("main table.data"), 6000);
  check("Users tab: the user list, and the settings form is hidden", /admin/.test(admin.$("main table.data").textContent) && !!admin.$("form.settings-form").closest("[hidden]"));
  admin.click(subtab("DICOM nodes")); await until(() => admin.$$("main h2").some((e) => e.textContent === "DICOM nodes"), 6000);
  check("DICOM nodes tab", !!admin.button(admin.$("main"), "Add node"));
  admin.click(subtab("Audit log")); await until(() => admin.$("main table.data") && admin.$$("main h2").some((e) => e.textContent === "Audit log"), 6000);
  check("Audit log tab lists entries", /login/.test(admin.$("main table.data").textContent));
  admin.click(subtab("Orthanc"));
  check("a value typed in a form tab survives switching tabs", admin.labelInput(admin.section("Orthanc connection"), "Orthanc address").value === "http://typed-not-saved:8042");
  await admin.tab("Studies"); await admin.tab("Settings"); await until(() => admin.$("form.settings-form"), 6000);
  check("...and is dropped (not saved) when leaving Settings", admin.labelInput(admin.section("Orthanc connection"), "Orthanc address").value === "http://127.0.0.1:18042");
  check("Settings reopens on the last tab used", subtab("Orthanc").getAttribute("aria-selected") === "true");
  await admin.tab("Studies"); await until(() => admin.$("tr.study"), 6000);
  const row = admin.$("tr.study");
  check("study row: preview, modalities, patient", !!row.querySelector(".thumb") && /DOE, JOHN/.test(row.textContent) && /CT/.test(row.textContent));
  check("a Weasis button on the study (desktop viewer)", !!admin.button(row, "Weasis"));
  const ohifButton = admin.button(row, "OHIF");
  check("...and a separate OHIF button (browser viewer, new tab)", !!ohifButton && /\/ohif\/viewer\?StudyInstanceUIDs=/.test(ohifButton.getAttribute("href"))
    && ohifButton.getAttribute("target") === "_blank" && ohifButton.getAttribute("rel") === "noopener");
  check("the Studies page is the Search page's first tab", admin.$(".subtabs [aria-selected=true]").textContent === "Studies");
  check("no accession number: neither a search box nor a column",
    !admin.$("input[name=accession_number]") && !admin.$$("main th").some((th) => /Accession/.test(th.textContent)) && !/ACC1/.test(row.textContent));
  const tagTitle = (code) => { const tag = Array.from(row.querySelectorAll(".chip")).find((c) => c.textContent === code); return tag && tag.getAttribute("title"); };
  check("modality tags explain themselves on hover: CT (button description), SR (DICOM name)",
    tagTitle("CT") === "CT: Computed tomography" && tagTitle("SR") === "SR: Structured report", `${tagTitle("CT")} | ${tagTitle("SR")}`);
  check("the default modality buttons, in the default order", admin.$$(".mod-picker .chip-toggle").map((b) => b.textContent).join() === "CT,MR,US,RX,MG,XA,RF,NM,PT", admin.$$(".mod-picker .chip-toggle").map((b) => b.textContent).join());
  check("the study date never wraps", row.querySelectorAll("td.nowrap").length > 0);
  check("the old View drop-down is gone", !Array.from(row.querySelectorAll("summary")).some((s) => s.textContent === "View"));
  check("the Weasis notice is no longer on the Studies page", !/Weasis needs to be/.test(admin.$("main").textContent));
  check("Send, Share, Download, Weasis and OHIF controls on every study", ["Send to...", "Share...", "Download", "Weasis", "OHIF"].every((n) => !!admin.button(row, n)), admin.words(row));
  check("...each an icon that says what it does on hover", ["Send to...", "Share...", "Download", "Weasis", "OHIF"].every((n) => { const b = admin.button(row, n); return b.querySelector("svg") && b.getAttribute("data-tip") && b.getAttribute("aria-label"); }));
  check("the old internet-viewing switch is gone from the Network settings", !/Internet viewers cannot download/.test((await (await admin.call("GET", "/api/admin/settings")).text())));

  console.log("--- sharing: not set up yet");
  admin.click(admin.button(row, "Share..."));
  await until(() => admin.$("dialog.share-dialog"));
  let dialog = admin.$("dialog.share-dialog");
  check("the dialog says sharing is not set up and offers the settings to an administrator", /Sharing is not set up yet/.test(dialog.textContent) && !!admin.button(dialog, "Open settings"), dialog.textContent);
  admin.click(admin.button(dialog, "Open settings"));
  await until(() => admin.section(SHARE_SECTION), 6000);
  const shareSection = admin.section(SHARE_SECTION);
  check("Settings has a Share links section with the defaults 48 hours and 1 download", !!shareSection
    && admin.labelInput(shareSection, "Link validity, default").value === "48" && admin.labelInput(shareSection, "Downloads allowed, default").value === "1");
  check("...and the dialog is gone", !admin.$("dialog.share-dialog"));
  check("...opened on the Network & sharing tab, where the Share links section is visible",
    admin.$(".subtabs [aria-selected=true]").textContent === "Network & sharing" && !shareSection.hidden);
  admin.labelInput(shareSection, "Public address of the share links").value = "https://pacs.example.com";
  admin.submit(admin.$("form.settings-form")); await sleep(1800);
  check("saving the public address works (the server keeps it)", (await (await admin.call("GET", "/api/admin/settings")).json()).settings.share_public_url === "https://pacs.example.com");

  console.log("--- Orthanc connection: with or without an account");
  // How the fake Orthanc protects itself can be switched while the tests run (see mock_orthanc.py).
  const mockAuth = (mode) => fetch("http://127.0.0.1:18042/__mock/auth", { method: "POST", body: JSON.stringify({ mode }) });
  const savedSettings = async () => (await (await admin.call("GET", "/api/admin/settings")).json()).settings;
  const orthancBits = () => {
    const sec = admin.section("Orthanc connection");
    return { sec, mode: sec.querySelector("#orthanc-mode"), user: admin.labelInput(sec, "Orthanc user name"), pass: admin.labelInput(sec, "Orthanc password"),
      test: admin.button(sec, "Test connection"), result: sec.querySelector(".test-result") };
  };
  const setMode = (el, value) => { el.value = value; el.dispatchEvent(new admin.window.Event("change", { bubbles: true })); };
  let o = orthancBits();
  check("the section asks how Orthanc lets the portal in, and shows the saved account", !!o.mode && o.mode.value === "account" && o.user.value === "orthanc" && !o.user.closest(".setting").hidden);
  setMode(o.mode, "anonymous");
  check("choosing 'without an account' hides the user name and password", o.user.closest(".setting").hidden && o.pass.closest(".setting").hidden);
  admin.click(o.test); await until(() => o.result.textContent.length > 12 && !/Testing/.test(o.result.textContent));
  check("Test connection without an account against an Orthanc that wants one: refused, with advice", /bad/.test(o.result.className) && /connect without an account/.test(o.result.textContent) && /Enter the account/.test(o.result.textContent), o.result.textContent);
  await mockAuth("none");
  admin.click(o.test); await until(() => /Connected/.test(o.result.textContent));
  check("...and against an Orthanc that has no users: connected, saying so", /ok/.test(o.result.className) && /without an account/.test(o.result.textContent), o.result.textContent);
  check("...testing changed nothing on the server", (await savedSettings()).orthanc_username === "orthanc");
  admin.submit(admin.$("form.settings-form")); await sleep(1800);
  let saved = await savedSettings();
  check("saving 'without an account' stores an empty user name and no password", saved.orthanc_username === "" && saved.orthanc_password_set === false, JSON.stringify([saved.orthanc_username, saved.orthanc_password_set]));
  o = orthancBits();
  check("the form comes back in 'without an account' mode, account fields hidden", o.mode.value === "anonymous" && o.user.closest(".setting").hidden);
  setMode(o.mode, "account");
  admin.submit(admin.$("form.settings-form")); await sleep(600);
  check("'with an account' and no user name is not saved: the user is told", /Enter the Orthanc user name/.test(admin.$$(".toast.error").map((e) => e.textContent).join()) && (await savedSettings()).orthanc_username === "");
  o = orthancBits(); setMode(o.mode, "account");
  o.user.value = "orthanc"; o.pass.value = "s3cr3tpw";
  await mockAuth("basic");
  admin.submit(admin.$("form.settings-form")); await sleep(1800);
  saved = await savedSettings();
  check("saving an account again keeps the user name and the (hidden) password", saved.orthanc_username === "orthanc" && saved.orthanc_password_set === true);
  check("...and the portal works against an Orthanc that wants that account", (await admin.call("GET", "/api/studies")).status === 200);

  console.log("--- sharing: the dialog (a staff member invites somebody)");
  await admin.tab("Studies");
  admin.click(admin.button(admin.$("tr.study"), "Share..."));
  await until(() => admin.$("dialog.share-dialog form"));
  dialog = admin.$("dialog.share-dialog");
  const [daysInput, hoursInput, minutesInput, downloadsInput] = dialog.querySelectorAll("input[type=number]");
  check("the form is ready at once after saving the address, with the defaults 2 days (48 hours) and 1 download",
    !!downloadsInput && daysInput.value === "2" && hoursInput.value === "0" && minutesInput.value === "0" && downloadsInput.value === "1", dialog.textContent);
  check("the form names the study", /DOE, JOHN/.test(dialog.textContent));
  dialog.querySelector("input[maxlength]").value = "Dr. Who";
  admin.submit(dialog.querySelector("form"));
  await until(() => dialog.querySelector(".password"));
  const url = dialog.querySelector(".secret-box input").value, password = dialog.querySelector(".password").textContent;
  const token = url.split("/s/")[1];
  check("the link is <public address>/s/<code>", /^https:\/\/pacs\.example\.com\/s\/[0-9a-f]{48}$/.test(url), url);
  check("the password has 3 groups of 4 readable characters", /^[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}$/.test(password), password);
  check("copy buttons for the link, the password and the invitation text", ["Copy link", "Copy password", "Copy invitation text (without the password)"].every((n) => !!admin.button(dialog, n)));
  check("the dialog says the password can be seen again in the Shares list", /see it again in the Shares list/.test(dialog.textContent));
  check("...and the validity and downloads are stated", /Downloads allowed: 1/.test(dialog.textContent) && /Valid until/.test(dialog.textContent));
  admin.click(admin.button(dialog, "Close"));
  check("closing removes the dialog", !admin.$("dialog.share-dialog"));

  console.log("--- the Shares tab");
  await admin.tab("Shares");
  const shareRows = admin.$$("table.data tbody tr");
  check("the link is listed: who, time left, downloads 0 of 1, never opened, active", shareRows.length === 1 && /admin/.test(shareRows[0].textContent) && /1 d 23 h/.test(shareRows[0].textContent) && /0 of 1/.test(shareRows[0].textContent) && /never/.test(shareRows[0].textContent) && /active/.test(shareRows[0].textContent), shareRows[0] && shareRows[0].textContent);
  check("...with the note, and Copy link / Revoke actions", /Dr\. Who/.test(shareRows[0].textContent) && !!admin.button(shareRows[0], "Copy link") && !!admin.button(shareRows[0], "Revoke"));
  check("...and never the password", !admin.text().includes(password) && !admin.text().includes(password.replaceAll("-", "")));

  console.log("--- the guest opens the link");
  const guest = await makeDom(GUEST, `/s/${token}`, SHARE);
  await until(() => guest.$("#share-password"));
  check("first thing: only a password form, no patient data", !!guest.$("#share-password") && /A study has been shared with you/.test(guest.text()) && !/DOE/.test(guest.text()));
  check("the page has no staff controls (no icons, no search)", !guest.$(".side-btn") && !guest.$(".chip-toggle") && !/weasis/i.test(guest.text()));
  guest.$("#share-password").value = "WRNG-WRNG-WRNG";
  guest.submit(guest.$("form.share-password"));
  await until(() => guest.$(".form-error") && !guest.$(".form-error").hidden);
  check("a wrong password shows an error and keeps the form", !!guest.$("#share-password") && /not correct/.test(guest.$(".form-error").textContent), guest.text());
  guest.$("#share-password").value = password.toLowerCase().replaceAll("-", " ");
  guest.submit(guest.$("form.share-password"));
  await until(() => guest.$("dl.share-details"));
  const details = guest.$("dl.share-details").textContent;
  check("the right password (typed in lower case, with spaces) shows the study", /DOE, JOHN/.test(details) && /P-001/.test(details) && /2024/.test(details) && /May/.test(details), details);
  check("study description, accession and modalities are shown", /CT CHEST/.test(details) && /ACC1/.test(details) && /CT/.test(details) && /SR/.test(details));
  check("the description is plain text: nothing from the server was run", /<script>alert\(1\)<\/script>/.test(details) && guest.$$("script").length === 0 && guest.errors.length === 0);
  const seriesRows = guest.$$("table.data tbody tr");
  check("series details: 2 series with number, modality, description, body part and image count", seriesRows.length === 2 && /2.*CT.*Chest 1mm.*CHEST.*3/.test(seriesRows[0].textContent) && /9.*SR.*Dose report.*5/.test(seriesRows[1].textContent), seriesRows.map((r) => r.textContent).join(" / "));
  const viewLink = guest.$(".share-actions a.primary");
  check("'View online' opens the browser viewer for this study in a new tab", !!viewLink && viewLink.textContent === "View online" && viewLink.getAttribute("href") === `/ohif/viewer?StudyInstanceUIDs=${STUDY_UID}` && viewLink.getAttribute("target") === "_blank" && viewLink.getAttribute("rel") === "noopener");
  const downloadLink = guest.$(".share-actions a[download]");
  check("'Download (ZIP)' is a plain link to the counted download", !!downloadLink && downloadLink.textContent === "Download (ZIP)" && downloadLink.getAttribute("href") === `/api/share/${token}/download`);
  check("remaining time is shown (about 48 hours)", /^1 d 23 h \d+ min$/.test(guest.$("#share-remaining").textContent), guest.$("#share-remaining").textContent);
  check("downloads left: 1 of 1", guest.$("#share-downloads").textContent === "1 of 1");
  check("a note says viewing continues after the downloads run out", /even when no downloads are left/.test(guest.text()));

  console.log("--- the guest downloads: the counter goes down, viewing stays");
  downloadLink.addEventListener("click", (e) => e.preventDefault()); // jsdom cannot navigate; the real request is made below
  guest.click(downloadLink);
  const zip = await guest.window.fetch(`/api/share/${token}/download`);
  const zipSize = (await zip.arrayBuffer()).byteLength;
  check("the download is a ZIP", zip.status === 200 && /zip/.test(zip.headers.get("content-type")) && zipSize > 1000, `${zip.status} ${zipSize}`);
  check("a few seconds later the page shows no downloads left", await until(() => /No downloads left/.test(guest.text()), 9000), guest.text().slice(-300));
  check("...'Download' is disabled, the counter says 0 of 1", !!guest.button(guest.window.document, "No downloads left") && guest.button(guest.window.document, "No downloads left").disabled && guest.$("#share-downloads").textContent === "0 of 1");
  check("...and 'View online' still works", !!guest.$(".share-actions a.primary") && guest.$(".share-actions a.primary").getAttribute("href").includes(STUDY_UID));
  check("a second download is refused by the server", (await guest.window.fetch(`/api/share/${token}/download`)).status === 403);

  console.log("--- the Shares tab follows, and the link can be revoked from it");
  await admin.tab("Shares");
  const row1 = admin.$$("table.data tbody tr")[0];
  check("downloads 1 of 1 and opened by a guest", /1 of 1/.test(row1.textContent) && /\dx, last/.test(row1.textContent), row1.textContent);
  admin.click(admin.button(row1, "Revoke"));
  await until(() => /revoked/.test(admin.$$("table.data tbody tr")[0].textContent), 5000);
  const revokedRow = admin.$$("table.data tbody tr")[0];
  check("Revoke marks it revoked and removes the actions", /revoked/.test(revokedRow.textContent) && !admin.button(revokedRow, "Revoke") && !admin.button(revokedRow, "Copy link"), revokedRow.textContent);
  check("the guest who was looking at it is cut off at once (410)", (await guest.window.fetch(`/api/share/${token}/study`)).status === 410);
  const revokedPage = await guestOpens(token);
  check("opening a revoked link says it is not available", /This link is not available/.test(revokedPage.text()) && !revokedPage.$("#share-password"), revokedPage.text());

  console.log("--- a view-only link (0 downloads)");
  const viewOnly = await newLink(admin, { hours: 24, downloads: 0 });
  const viewOnlyPage = await guestOpens(viewOnly.token, viewOnly.password);
  check("'Download not enabled' is disabled and the counter says online viewing only", !!viewOnlyPage.button(viewOnlyPage.window.document, "Download not enabled") && viewOnlyPage.button(viewOnlyPage.window.document, "Download not enabled").disabled && /online viewing only/.test(viewOnlyPage.$("#share-downloads").textContent), viewOnlyPage.text());
  check("...and it can still be viewed", !!viewOnlyPage.$(".share-actions a.primary"));
  check("...remaining time about 24 hours", /^23 h \d+ min$/.test(viewOnlyPage.$("#share-remaining").textContent), viewOnlyPage.$("#share-remaining").textContent);

  console.log("--- the countdown, and the end of the link");
  const shortLink = await newLink(admin, { hours: 1, downloads: 1 });
  // 12 s: checking the password (Argon2, slow in a debug build) takes a couple of seconds of that
  sql(`update shares set expires_at=${nowSeconds() + 12} where token='${shortLink.token}'`);
  const shortPage = await guestOpens(shortLink.token, shortLink.password);
  const first = shortPage.$("#share-remaining") && shortPage.$("#share-remaining").textContent;
  check("with seconds to go the countdown is in seconds", /^([2-9]|1[0-2]) s$/.test(first), first);
  await sleep(2600);
  const later = shortPage.$("#share-remaining") && shortPage.$("#share-remaining").textContent;
  check("...and it counts down by itself", /^\d+ s$/.test(later) && parseInt(later) < parseInt(first), `${first} -> ${later}`);
  check("when the time is up the page says the link has expired", await until(() => /This link has expired/.test(shortPage.text()), 15000), shortPage.text());
  check("...and the study details are gone from the page", !/DOE/.test(shortPage.text()) && !shortPage.$("dl.share-details"));
  const expiredPage = await guestOpens(shortLink.token);
  check("opening an expired link says so, without asking for the password", /This link has expired/.test(expiredPage.text()) && !expiredPage.$("#share-password"), expiredPage.text());
  await admin.tab("Studies");
  await admin.tab("Shares");
  check("the Shares tab shows it as expired", /expired/.test(admin.text()), "");

  console.log("--- other addresses on the internet listener");
  const nothing = await makeDom(GUEST, "/", SHARE);
  await until(() => nothing.$(".share-card"));
  check("the bare address only asks for the invitation link", /Open your invitation link/.test(nothing.text()) && !nothing.$("#share-password"));
  const garbage = await makeDom(GUEST, "/s/not-a-link", SHARE);
  await until(() => garbage.$(".share-card"));
  check("a malformed link: same message", /Open your invitation link/.test(garbage.text()));
  const unknown = await guestOpens("f".repeat(48));
  check("an unknown link: not available", /This link is not available/.test(unknown.text()));
  check("the page shows the practice's branding header", !!nothing.$(".share-head .brand-name") && nothing.$(".share-head .brand-name").textContent.length > 0);

  console.log("--- the online viewer is optional");
  await admin.call("PUT", "/api/admin/settings", { ohif: "off" });
  const noViewerLink = await newLink(admin, {});
  const noViewerPage = await guestOpens(noViewerLink.token, noViewerLink.password);
  const disabledView = noViewerPage.button(noViewerPage.window.document, "View online");
  check("without OHIF the 'View online' button is disabled and explains why", !!disabledView && disabledView.disabled && /not available/.test(disabledView.getAttribute("title")), noViewerPage.text());
  const adminNoOhif = await makeDom(STAFF);
  await adminNoOhif.login("admin", "SuperSecret123");
  await sleep(1000);
  await until(() => adminNoOhif.$("tr.study"), 6000);
  check("on the intranet Weasis is still there when OHIF is off", !!adminNoOhif.button(adminNoOhif.$("tr.study"), "Weasis"));
  check("...and the OHIF button is gone", !adminNoOhif.button(adminNoOhif.$("tr.study"), "OHIF"));
  await admin.call("PUT", "/api/admin/settings", { ohif: "auto" });

  console.log("--- the Weasis button can be switched off in Settings");
  const viewers = await makeDom(STAFF);
  await viewers.login("admin", "SuperSecret123");
  await viewers.tab("Settings");
  await until(() => viewers.$("form.settings-form"), 6000);
  viewers.click(viewers.$$(".subtabs [role=tab]").find((b) => b.textContent === "General"));
  const viewerSection = viewers.section("Viewers and uploads");
  const weasisToggle = viewers.labelInput(viewerSection, "Show the Weasis button");
  check("Settings > General > Viewers has the Weasis switch, on by default", !!weasisToggle && weasisToggle.checked && !viewerSection.hidden);
  check("...with the notice that Weasis must be installed and OHIF needs nothing", /Weasis needs to be installed on each computer that uses it; the browser viewer \(OHIF\) works without installing anything/.test(weasisToggle.closest(".setting").textContent));
  check("...and the OHIF switch next to it", !!viewers.labelInput(viewerSection, "Show the OHIF button"));
  weasisToggle.checked = false;
  viewers.submit(viewers.$("form.settings-form")); await sleep(1800);
  check("switching Weasis off is saved", (await (await admin.call("GET", "/api/admin/settings")).json()).settings.weasis_enabled === false);
  await viewers.tab("Studies"); await until(() => viewers.$("tr.study"), 6000);
  check("...and the Weasis button disappears at once (no new sign-in), OHIF stays",
    !viewers.button(viewers.$("tr.study"), "Weasis") && !!viewers.button(viewers.$("tr.study"), "OHIF"));
  await admin.call("PUT", "/api/admin/settings", { weasis_enabled: true });

  console.log("--- modality buttons: order and visibility chosen in Settings");
  await viewers.tab("Settings");
  await until(() => viewers.section("Modality buttons"), 6000);
  viewers.click(viewers.$$(".subtabs [role=tab]").find((b) => b.textContent === "Search & labels"));
  const modSection = viewers.section("Modality buttons");
  const modRows = () => Array.from(viewers.section("Modality buttons").querySelectorAll(".modbtn-row"));
  const rowOf = (label) => modRows().find((r) => r.querySelector(".modbtn-label").value === label);
  const previewChips = () => Array.from(viewers.section("Modality buttons").querySelectorAll(".modbtn-preview .chip-toggle")).map((b) => b.textContent).join();
  check("Settings > Search & labels has the modality buttons editor, with the 9 defaults", !modSection.hidden && modRows().length === 9 && previewChips() === "CT,MR,US,RX,MG,XA,RF,NM,PT", previewChips());
  check("...and its preview always shows the Other codes box", !!modSection.querySelector(".modbtn-preview .mod-other"));
  for (let i = 0; i < 3; i++) viewers.click(viewers.button(rowOf("RX"), "↑")); // RX to the top
  const shownBox = (label) => rowOf(label).querySelector("input[type=checkbox]");
  const toggle = (label, on) => { const box = shownBox(label); box.checked = on; box.dispatchEvent(new viewers.window.Event("change", { bubbles: true })); };
  toggle("CT", false);
  check("moving up and hiding update the preview at once", previewChips() === "RX,MR,US,MG,XA,RF,NM,PT", previewChips());
  viewers.submit(viewers.$("form.settings-form")); await sleep(1800);
  let savedButtons = (await (await admin.call("GET", "/api/admin/settings")).json()).settings.modality_buttons;
  check("the order and the hidden button are saved (the hidden one is kept, to show it again later)",
    savedButtons.map((b) => b.label + (b.visible ? "" : "-")).join() === "RX,CT-,MR,US,MG,XA,RF,NM,PT", savedButtons.map((b) => b.label + (b.visible ? "" : "-")).join());
  await viewers.tab("Studies"); await until(() => viewers.$("tr.study"), 6000);
  check("the Studies page follows at once: RX first, no CT", viewers.$$(".mod-picker .chip-toggle").map((b) => b.textContent).join() === "RX,MR,US,MG,XA,RF,NM,PT", viewers.$$(".mod-picker .chip-toggle").map((b) => b.textContent).join());
  await viewers.tab("Series"); await until(() => viewers.$(".mod-picker"), 6000);
  check("...and so does the Series page", viewers.$$(".mod-picker .chip-toggle").map((b) => b.textContent).join() === "RX,MR,US,MG,XA,RF,NM,PT");
  await admin.call("PUT", "/api/admin/settings", { modality_buttons: savedButtons.map((b) => ({ ...b, visible: false })) });
  const noButtons = await makeDom(STAFF);
  await noButtons.login("admin", "SuperSecret123");
  await noButtons.tab("Studies");
  await until(() => noButtons.$(".mod-picker"), 6000);
  check("with every button hidden, the Other codes box is still there", noButtons.$$(".mod-picker .chip-toggle").length === 0 && !!noButtons.$(".mod-picker .mod-other"));
  const defaultButtons = (await (await admin.call("GET", "/api/admin/settings")).json()).defaults.modality_buttons;
  await admin.call("PUT", "/api/admin/settings", { modality_buttons: defaultButtons });

  console.log("--- staff accounts: who sees what");
  await admin.call("POST", "/api/admin/users", { username: "sharer", password: "SharerPass123", permissions: ["read", "visualize", "share"] });
  const sharer = await makeDom(STAFF);
  await sharer.login("sharer", "SharerPass123");
  await sleep(1200);
  check("sharer: Home, Search and Shares only", sharer.tabs().join() === "Home,Search,Shares,My account,Help", sharer.tabs().join());
  await until(() => sharer.$("tr.study"), 6000);
  const srow = sharer.$("tr.study");
  check("sharer: Share... and the viewers, but no Send or Download", ["Share...", "Weasis", "OHIF"].every((n) => !!sharer.button(srow, n)) && !sharer.button(srow, "Send to...") && !sharer.button(srow, "Download"), sharer.words(srow));
  check("sharer: no upload area on the home page", !sharer.$(".home-drop"));
  await sharer.tab("Shares");
  check("sharer: sees none of the administrators' links", /No invitation yet/.test(sharer.text()), sharer.text().slice(-200));
  await admin.call("POST", "/api/admin/users", { username: "viewer1", password: "ViewerPass123", permissions: ["read", "visualize"] });
  const viewer = await makeDom(STAFF);
  await viewer.login("viewer1", "ViewerPass123");
  await sleep(1200);
  await until(() => viewer.$("tr.study"), 6000);
  check("viewer: Home and Search, no Share button, no Shares page", viewer.tabs().join() === "Home,Search,My account,Help" && !viewer.button(viewer.$("tr.study"), "Share..."), viewer.tabs().join());
  await admin.call("POST", "/api/admin/users", { username: "reader", password: "ReaderPass123", permissions: ["read"] });
  const reader = await makeDom(STAFF);
  await reader.login("reader", "ReaderPass123");
  await sleep(1000);
  await until(() => reader.$("tr.study"), 6000);
  check("read-only account: the list, no Download, no Share, no viewers, no previews", reader.$$("tr.study").length === 1 && !reader.$("details.menu") && !reader.button(reader.$("tr.study"), "Share...") && !reader.button(reader.$("tr.study"), "Weasis") && !reader.$(".thumb"));

  console.log("--- labels, sorting, the patient's other studies, links, live search, endless lists, chosen columns");
  const findLog = () => JSON.parse(fs.readFileSync(path.join(LOGS, "find.log"), "utf8").trim().split("\n").pop());
  await admin.tab("Studies"); await until(() => admin.$("tr.study"), 6000);
  admin.click(admin.button(admin.$("tr.study"), "Labels"));
  await until(() => admin.$("dialog .label-list"));
  const labelsDialog = admin.$$("dialog").pop();
  labelsDialog.querySelector("input[list]").value = "second-opinion";
  admin.submit(labelsDialog.querySelector("form"));
  await until(() => /second-opinion/.test(labelsDialog.querySelector(".label-list").textContent), 5000);
  check("the Labels button puts a label on the study", /second-opinion/.test(labelsDialog.querySelector(".label-list").textContent));
  check("...and the row shows it at once", /second-opinion/.test(admin.$("tr.study .labels-cell").textContent));
  labelsDialog.querySelector("input[list]").value = "has space";
  admin.submit(labelsDialog.querySelector("form"));
  await until(() => !labelsDialog.querySelector(".form-error").hidden, 5000);
  check("a label with a space is refused, with the reason", /letters, digits/.test(labelsDialog.querySelector(".form-error").textContent), labelsDialog.querySelector(".form-error").textContent);
  admin.click(admin.button(labelsDialog, "Close"));
  await admin.tab("Studies"); await until(() => admin.$$(".label-picker .chip-toggle").some((b) => /second-opinion/.test(b.textContent)), 6000);
  const labelToggle = admin.$$(".label-picker .chip-toggle").find((b) => /second-opinion/.test(b.textContent));
  check("the Labels search box offers it, with its number of studies", labelToggle.textContent === "second-opinion (1)", labelToggle.textContent);
  admin.click(labelToggle); await sleep(1200);
  check("clicking it searches the studies with that label", JSON.stringify(findLog().Labels) === '["second-opinion"]', JSON.stringify(findLog()));
  check("...and the page address keeps the search (it can be bookmarked)", /labels=second-opinion/.test(admin.window.location.search), admin.window.location.search);
  admin.click(admin.button(admin.$("form.filters"), "Clear filters")); await sleep(1200);
  const patientTitle = () => admin.$$("th .sort-header").find((b) => /^Patient( [▲▼])?$/.test(b.textContent));
  admin.click(patientTitle()); await sleep(1200);
  check("clicking a column title sorts by it", findLog().OrderBy[0].Key === "PatientName" && findLog().OrderBy[0].Direction === "ASC", JSON.stringify(findLog().OrderBy));
  admin.click(patientTitle()); await sleep(1200);
  check("...a second click the other way round, with an arrow", findLog().OrderBy[0].Direction === "DESC" && /▼/.test(admin.$("th[aria-sort] .sort-header").textContent));
  check("columns without a value to sort by have a plain title", !admin.$$("th .sort-header").some((b) => /Modality|Labels/.test(b.textContent)));
  admin.click(admin.button(admin.$("tr.study"), "Other studies of this patient")); await sleep(1200);
  check("'Other studies of this patient' searches its exact Patient ID", admin.$("input[name=patient_id]").value === '"P-001"' && findLog().Query.PatientID === "P-001", JSON.stringify(findLog().Query));
  check("...on the Search page", admin.$(".subtabs [aria-selected=true]").textContent === "Studies");

  const linked = await makeDom(STAFF, "/?patient_id=P-001&sort=patient&dir=asc");
  await linked.login("admin", "SuperSecret123"); await until(() => linked.$("th[aria-sort]"), 6000);
  check("a link with filters opens the Search page with them, after signing in", !!linked.$("input[name=patient_id]") && linked.$("input[name=patient_id]").value === "P-001" && /Patient ▲/.test(linked.$("th[aria-sort]").textContent));
  const viaUid = await makeDom(STAFF, `/?study_uid=${STUDY_UID}`);
  await viaUid.login("admin", "SuperSecret123"); await until(() => viaUid.$("input[name=study_uid]"), 6000);
  check("...a filter without a box of its own (StudyInstanceUID) gets one, so it is never hidden", !!viaUid.$("input[name=study_uid]") && viaUid.$("input[name=study_uid]").value === STUDY_UID);

  await admin.call("PUT", "/api/admin/settings", { search_as_you_type: true, infinite_scroll: true });
  await fetch("http://127.0.0.1:18042/__mock/copies", { method: "POST", body: JSON.stringify({ n: 30 }) });
  const live = await makeDom(STAFF);
  await live.login("admin", "SuperSecret123"); await live.tab("Studies"); await until(() => live.$$("tr.study").length >= 25, 6000);
  check("endless list: the first 25, no Previous / Next, a Load more button", live.$$("tr.study").length === 25 && !live.button(live.$("main"), "Next") && !!live.button(live.$("main"), "Load more"), String(live.$$("tr.study").length));
  live.click(live.button(live.$("main"), "Load more")); await until(() => live.$$("tr.study").length === 30, 6000);
  check("...Load more adds the next ones below", live.$$("tr.study").length === 30 && /That is all/.test(live.$(".load-more").textContent), String(live.$$("tr.study").length));
  const nameBox = live.$("input[name=patient_name]");
  nameBox.value = "DO"; nameBox.dispatchEvent(new live.window.Event("input", { bubbles: true })); await sleep(900);
  check("live search: two letters are not enough yet (3 by default)", findLog().Query.PatientName !== "*DO*");
  nameBox.value = "DOE"; nameBox.dispatchEvent(new live.window.Event("input", { bubbles: true })); await sleep(1200);
  check("...with three, the results follow without pressing Search", findLog().Query.PatientName === "*DOE*", JSON.stringify(findLog().Query));
  await fetch("http://127.0.0.1:18042/__mock/copies", { method: "POST", body: JSON.stringify({ n: 1 }) });
  await admin.call("PUT", "/api/admin/settings", { search_as_you_type: false, infinite_scroll: false });

  { // its own scope: the names below are used again further on
  console.log("--- optional features (Settings > Features)");
  const fresh = async () => { const t = await makeDom(STAFF); await t.login("admin", "SuperSecret123"); await t.tab("Studies"); await until(() => t.$("tr.study"), 6000); return t; };
  let f = await fresh();
  check("by default: tick boxes on the studies list", !!f.$("tr.study input.pick") && !!f.$("thead input.pick-all"));
  check("...no Manage menu while delete / modify are off", !f.$("tr.study [data-name=Manage]"));
  check("...no Retrieve or Worklists page", !f.tabs().includes("Retrieve") && !f.tabs().includes("Worklists"));
  const pick = f.$("tr.study input.pick");
  pick.checked = true; pick.dispatchEvent(new f.window.Event("change", { bubbles: true }));
  const bar = f.$(".bulk-bar");
  check("ticking a study shows the bar of actions", !!bar && !bar.hidden && /1 selected/.test(bar.textContent), bar && bar.textContent);
  check("...with one ZIP for the ticked studies", !!bar.querySelector(`a[href="/api/download?studies=${SID}"]`));
  check("...send, share (one link), labels, clear", ["Send...", "Share (one link)...", "Labels...", "Clear"].every((t) => f.button(bar, t)));
  f.click(f.button(bar, "Clear"));
  check("...Clear unticks and hides the bar", !pick.checked && bar.hidden);
  f.click(f.$("tr.study .twisty")); await until(() => f.$("tr.series-row [data-name=Images]"), 6000);
  check("a series offers its images", !!f.$("tr.series-row [data-name=Images]"));
  f.click(f.$("tr.series-row [data-name=Images]")); await until(() => f.$("dialog .instance-list button"), 6000);
  check("...the images of the series are listed", f.$$("dialog .instance-list button").length === 3, String(f.$$("dialog .instance-list button").length));
  f.click(f.button(f.$("dialog .instance-view"), "DICOM tags")); await until(() => f.$("dialog .tags-table tbody tr"), 6000);
  const tagText = f.$("dialog .tags-table").textContent;
  check("...an image's DICOM tags, sequences included", /PatientName/.test(tagText) && /DOE\^JOHN/.test(tagText) && /CodeValue/.test(tagText), tagText.slice(0, 200));
  const tagFilter = f.$("dialog .instance-view input");
  tagFilter.value = "modality"; tagFilter.dispatchEvent(new f.window.Event("input"));
  check("...the filter box narrows them", f.$$("dialog .tags-table tbody tr").length === 1, String(f.$$("dialog .tags-table tbody tr").length));
  f.$("dialog").close();
  f.window.close();

  await admin.call("PUT", "/api/admin/settings", { labels_enabled: false, bulk_actions: false, date_format: "dmy", name_format: "first_last" });
  f = await fresh();
  const heads = f.$$("table.data thead th").map((th) => th.textContent.replace(/[▲▼]/g, "").trim()).join();
  check("labels switched off: no Labels column", !/Labels/.test(heads), heads);
  check("...no labels box in the search", !f.$(".label-picker"));
  check("...no labels button on a study", !f.$("tr.study [data-name=Labels]"));
  check("bulk actions off: no tick boxes", !f.$("input.pick") && !f.$(".bulk-bar"));
  check("dates follow the chosen format (17/05/2024)", /17\/05\/2024/.test(f.$("tr.study").textContent), f.$("tr.study").textContent);
  check("...names too (JOHN DOE)", /JOHN DOE/.test(f.$("tr.study").textContent));
  f.window.close();
  await admin.call("PUT", "/api/admin/settings", { labels_enabled: true, bulk_actions: true, date_format: "iso", name_format: "last_first" });

  await admin.call("PUT", "/api/admin/settings", { allow_delete: true, allow_modify: true, allow_anonymize: true, allow_add_series: true,
    query_retrieve: true, worklists_enabled: true, stone_viewer: "on", ohif_modes: ["viewer", "mpr"] });
  f = await fresh();
  const manage = f.$("tr.study [data-name=Manage]");
  check("switched on: a Manage menu on each study", !!manage);
  const items = manage ? Array.from(manage.parentElement.querySelectorAll(".menu-item")).map((b) => b.textContent) : [];
  check("...change, move, anonymize, add a series, delete", ["Change patient / study details...", "Move to another patient...", "Anonymize...", "Add a series (PDF, pictures)...", "Delete..."].every((t) => items.includes(t)), items.join());
  const moreViewers = f.$("tr.study [data-name='More viewers']");
  const viewerLinks = moreViewers ? Array.from(moreViewers.parentElement.querySelectorAll("a.menu-item")).map((a) => `${a.textContent}=${a.getAttribute("href")}`) : [];
  check("More viewers: the OHIF MPR mode and Stone", viewerLinks.some((l) => l.startsWith("OHIF: MPR=/ohif/viewer?hangingprotocolId=mpr&StudyInstanceUIDs=")) && viewerLinks.some((l) => l.startsWith(`Stone Web Viewer=/stone-webviewer/index.html?study=${STUDY_UID}`)), viewerLinks.join(" | "));
  f.click(Array.from(manage.parentElement.querySelectorAll(".menu-item")).find((b) => b.textContent.startsWith("Change patient")));
  await until(() => f.$("dialog input[data-tag=PatientName]"), 4000);
  const nameBox = f.$("dialog input[data-tag=PatientName]");
  check("...the change window is filled with the study's values", nameBox && nameBox.value === "DOE^JOHN");
  nameBox.value = "DOE^JANE";
  f.submit(f.$("dialog form")); await sleep(1500);
  const lastModify = JSON.parse(fs.readFileSync(path.join(LOGS, "modify.log"), "utf8").trim().split("\n").pop());
  check("...only the changed tag is sent to Orthanc", JSON.stringify(lastModify.body.Replace) === JSON.stringify({ PatientName: "DOE^JANE" }), JSON.stringify(lastModify.body.Replace));
  check("...the window closes and the job is followed", !f.$("dialog[open]") && /Changing the study/.test(f.text()));
  check("Retrieve and Worklists pages appear", f.tabs().includes("Retrieve") && f.tabs().includes("Worklists"), f.tabs().join());
  await f.tab("Retrieve"); await until(() => f.$("select[name=source] option[value=ris]"), 6000);
  f.$("select[name=source]").value = "ris";
  f.$("input[name=patient_name]").value = "DOE";
  f.submit(f.$("main form")); await until(() => f.$$("main table.data tbody tr").length === 2, 6000);
  const answers = f.$$("main table.data tbody tr").map((tr) => tr.textContent);
  check("Retrieve: the other system's studies", answers.length === 2 && /REMOTE/.test(answers[0]), answers.join(" | "));
  check("...one is already here (Open), the other can be retrieved", /Already here/.test(answers[1]) && !!f.button(f.$$("main table.data tbody tr")[0], "Retrieve"));
  f.click(f.button(f.$$("main table.data tbody tr")[0], "Retrieve")); await sleep(1200);
  await f.tab("Transfers"); await until(() => /REMOTE/.test((f.$("main table.data") || {}).textContent || ""), 6000);
  const retrievedRow = f.$$("main table.data tbody tr").find((tr) => /REMOTE/.test(tr.textContent));
  check("...Transfers says the study was retrieved (not sent), from that system", !!retrievedRow && /Retrieved/.test(retrievedRow.textContent) && !/Sent/.test(retrievedRow.textContent) && /from/.test(retrievedRow.textContent), retrievedRow && retrievedRow.textContent);
  check("...and offers to open it", !!retrievedRow && !!f.button(retrievedRow, "Open the study"));
  await f.tab("Worklists"); await until(() => f.button(f.$("main"), "Add a worklist"), 6000);
  f.click(f.button(f.$("main"), "Add a worklist")); await until(() => f.$("dialog input[name=patient_id]"), 4000);
  const wl = (name, value) => { f.$(`dialog input[name=${name}]`).value = value; };
  wl("patient_name", "DOE^JANE"); wl("patient_id", "P-77"); wl("modality", "us"); wl("date", "2026-11-20"); wl("time", "09:30");
  f.submit(f.$("dialog form")); await until(() => f.$$("main table.data tbody tr").length > 0, 6000);
  check("Worklists: a new one is listed", /P-77/.test(f.$("main table.data").textContent) && /09:30/.test(f.$("main table.data").textContent), f.$("main").textContent.slice(0, 300));
  f.window.close();
  await admin.call("PUT", "/api/admin/settings", { allow_delete: false, allow_modify: false, allow_anonymize: false, allow_add_series: false,
    query_retrieve: false, worklists_enabled: false, stone_viewer: "auto", ohif_modes: ["viewer"] });

  f = await makeDom(STAFF); await f.login("admin", "SuperSecret123");
  await f.tab("Settings"); await until(() => f.section("Optional features"), 6000);
  f.click(f.$$(".subtabs [role=tab]").find((b) => b.textContent === "Features"));
  const shownSections = f.$$("section.settings-section").filter((s) => !s.hidden).map((s) => s.querySelector("h2").textContent).join();
  check("Settings > Features: the optional features and the viewers", shownSections === "Optional features,More viewers", shownSections);
  f.click(f.$$(".subtabs [role=tab]").find((b) => b.textContent === "System")); await until(() => /Plugins \(/.test(f.text()), 6000);
  check("Configuration > System: version, counts and plugins", /1\.12\.10/.test(f.text()) && /Plugins \(3\)/.test(f.text()));
  await until(() => /Orthanc log/.test(f.text()) && /Settings > Orthanc/.test(f.$("#orthanc-log").textContent), 6000);
  check("...at the bottom, Orthanc's log: explains how to set it up", f.$$("main .panel").pop().id === "orthanc-log" && !!f.button(f.$("#orthanc-log"), "Open Settings > Orthanc"));
  const logDir = path.join(LOGS, "ui-orthanc-logs");
  fs.mkdirSync(logDir, { recursive: true });
  fs.writeFileSync(path.join(logDir, "Orthanc.log.20261118-100000.1"), "I1118 10:00:00.1 MAIN a] Started\nW1118 10:00:01.1 MAIN b] Careful\nE1118 10:00:02.1 MAIN c] Broken\n");
  await admin.call("PUT", "/api/admin/settings", { orthanc_log_dir: logDir });
  f.click(f.$$(".subtabs [role=tab]").find((b) => b.textContent === "Jobs")); await sleep(300);
  f.click(f.$$(".subtabs [role=tab]").find((b) => b.textContent === "System")); await until(() => f.$("#orthanc-log .log-line"), 6000);
  check("...set up: the lines, coloured by level", f.$$("#orthanc-log .log-line").length === 3 && !!f.$("#orthanc-log .log-E") && !!f.$("#orthanc-log .log-W"), f.$("#orthanc-log").textContent.slice(0, 200));
  const logLevel = f.$("#orthanc-log select[aria-label=Levels]");
  logLevel.value = "E"; logLevel.dispatchEvent(new f.window.Event("change"));
  check("...errors only", f.$$("#orthanc-log .log-line").length === 1 && /Broken/.test(f.$("#orthanc-log .log-box").textContent));
  logLevel.value = ""; logLevel.dispatchEvent(new f.window.Event("change"));
  const logSearch = f.$("#orthanc-log input[type=search]");
  logSearch.value = "careful"; logSearch.dispatchEvent(new f.window.Event("input"));
  check("...and a search box", f.$$("#orthanc-log .log-line").length === 1 && /Careful/.test(f.$("#orthanc-log .log-box").textContent));
  await admin.call("PUT", "/api/admin/settings", { orthanc_log_dir: "" });
  f.click(f.$$(".subtabs [role=tab]").find((b) => b.textContent === "Jobs")); await until(() => f.$("main table.data tbody tr") || /No job/.test(f.text()), 6000);
  check("Configuration > Jobs: Orthanc's jobs", !!f.$("main table.data tbody tr"), f.$("main").textContent.slice(0, 200));
  f.window.close();

  }

  console.log("--- auto-refresh");
  await admin.call("PUT", "/api/admin/settings", { auto_refresh_seconds: 5, recent_auto_refresh: true, transfers_auto_refresh: true });
  const auto = await makeDom(STAFF);
  let studySearches = 0; // the searches of this browser only
  const autoFetch = auto.window.fetch;
  auto.window.fetch = (url, opts) => { if (String(url).startsWith("/api/studies?")) studySearches += 1; return autoFetch(url, opts); };
  const findCount = () => studySearches;
  await auto.login("admin", "SuperSecret123"); await until(() => auto.$("tr.study"), 6000);
  const homeSwitch = auto.$(".recent-head [data-name=auto-refresh]");
  check("the home page has an Auto-refresh switch, on, counting down from the interval", !!homeSwitch && homeSwitch.getAttribute("aria-pressed") === "true" && /^Auto-refresh · ([1-5] s|refreshing)$/.test(homeSwitch.textContent), homeSwitch && homeSwitch.textContent);
  check("...its name for screen readers stays the same", homeSwitch.getAttribute("aria-label") === "Refresh automatically every 5 seconds");
  const secondsLeft = () => Number((/(\d+) s$/.exec(homeSwitch.textContent) || [])[1]);
  await until(() => secondsLeft() === 4, 7000);
  await sleep(1100);
  check("...the seconds left go down every second", secondsLeft() === 3, homeSwitch.textContent);
  await until(() => /refreshing/.test(homeSwitch.textContent), 5000);
  check("...and it says when the update happens", /refreshing/.test(homeSwitch.textContent), homeSwitch.textContent);
  let before = findCount(); await sleep(5800);
  check("...the recent studies refresh themselves", findCount() > before, `${before} -> ${findCount()}`);
  auto.click(homeSwitch);
  check("...a click pauses it (remembered in the browser)", homeSwitch.getAttribute("aria-pressed") === "false" && auto.window.localStorage.getItem("portal.autorefresh.recent") === "off");
  check("...and the button says so", homeSwitch.textContent === "Auto-refresh · paused", homeSwitch.textContent);
  before = findCount(); await sleep(5800);
  check("...and paused, nothing is asked any more", findCount() === before, `${before} -> ${findCount()}`);
  auto.click(homeSwitch);
  auto.click(auto.$("tr.study .twisty")); await sleep(800);
  before = findCount(); await sleep(5800);
  check("...while the series of a study are unfolded, it waits", !!auto.$("tr.series-row:not([hidden])") && findCount() === before, `${before} -> ${findCount()}`);
  check("...and the button says it is waiting", homeSwitch.textContent === "Auto-refresh · waiting", homeSwitch.textContent);
  auto.click(auto.$("tr.study .twisty"));
  before = findCount(); await sleep(5800);
  check("...and folded again, it goes on", findCount() > before, `${before} -> ${findCount()}`);
  await auto.tab("Transfers"); await until(() => auto.$(".page-head [data-name=auto-refresh]"), 6000);
  check("the Transfers page has its own switch, next to Refresh", !!auto.$(".page-head [data-name=auto-refresh]") && auto.$(".page-head [data-name=auto-refresh]").getAttribute("aria-pressed") === "true");
  await admin.call("PUT", "/api/admin/settings", { auto_refresh_seconds: 30, recent_auto_refresh: false, transfers_auto_refresh: false });
  const noAuto = await makeDom(STAFF);
  await noAuto.login("admin", "SuperSecret123"); await until(() => noAuto.$("tr.study"), 6000);
  const homeHasSwitch = !!noAuto.$("[data-name=auto-refresh]");
  await noAuto.tab("Transfers"); await sleep(800);
  check("switched off in Settings, no switch on either page", !homeHasSwitch && !noAuto.$("[data-name=auto-refresh]"));
  auto.window.close(); noAuto.window.close();

  await admin.call("PUT", "/api/admin/settings", { study_columns: ["patient", "accession", "labels"], study_search_fields: ["patient_name", "sex"] });
  const custom = await makeDom(STAFF);
  await custom.login("admin", "SuperSecret123"); await custom.tab("Studies"); await until(() => custom.$("tr.study"), 6000);
  const headerTitles = custom.$$("table.data thead th").map((th) => th.textContent.replace(/[▲▼]/g, "").trim()).filter(Boolean).join();
  check("the columns chosen in Settings, in their order", headerTitles === "Preview,Patient,Accession number,Labels,Actions", headerTitles);
  check("...and the search boxes chosen (a Sex list, no Patient ID box)", !!custom.$("select[name=patient_sex]") && !custom.$("input[name=patient_id]"));
  const searchDefaults = (await (await admin.call("GET", "/api/admin/settings")).json()).defaults;
  await admin.call("PUT", "/api/admin/settings", { study_columns: searchDefaults.study_columns, study_search_fields: searchDefaults.study_search_fields });

  console.log("--- translations: Spanish (the language menu, the portal's language, the invitation page)");
  {
    // Every text given to t() (also t(x ? "a" : "b")) in app.js and share.js has its Spanish translation.
    const es = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "static", "i18n", "es.json"), "utf8"));
    const unquote = (q) => JSON.parse(`"${q}"`);
    const keys = new Set();
    for (const m of (APP + SHARE).matchAll(/\bt\(\s*"((?:[^"\\]|\\.)*)"/g)) keys.add(unquote(m[1]));
    for (const m of (APP + SHARE).matchAll(/\bt\([^()"]*\?\s*"((?:[^"\\]|\\.)*)"\s*:\s*"((?:[^"\\]|\\.)*)"/g)) { keys.add(unquote(m[1])); keys.add(unquote(m[2])); }
    const missing = [...keys].filter((k) => !(k.trim() in es.texts));
    check(`every text given to t() has its Spanish translation (${keys.size} texts)`, keys.size > 100 && missing.length === 0, missing.slice(0, 8).join(" | "));
    const holders = (text) => [...text.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join();
    const badHolders = Object.entries({ ...es.texts, ...es.messages }).filter(([k, v]) => holders(k) !== holders(v)).map(([k]) => k);
    check("...with the same {names} in each translation", badHolders.length === 0, badHolders.slice(0, 5).join(" | "));

    const spanish = await makeDom(STAFF);
    await spanish.login("admin", "SuperSecret123");
    const menu = spanish.$(".simple-head select.language-select");
    check("a language menu in the header (English first)", !!menu && menu.value === "en" && menu.options.length === 2);
    menu.value = "es"; menu.dispatchEvent(new spanish.window.Event("change"));
    await until(() => spanish.window.document.documentElement.lang === "es" && spanish.$$(".side-btn").some((b) => b.dataset.tip === "Inicio"), 6000);
    check("...Spanish: the icons, the header and the home page", spanish.$$(".side-btn").map((b) => b.dataset.tip).includes("Inicio")
      && /Total de estudios/.test(spanish.$(".simple-head").textContent) && /Estudios recientes/.test(spanish.text()), spanish.$$(".side-btn").map((b) => b.dataset.tip).join());
    check("...remembered in this browser", spanish.window.localStorage.getItem("portal.language") === "es");
    check("...the icon buttons too (their tooltip and name for screen readers)", !!spanish.$("[data-tip='Actualizar los estudios recientes'][aria-label='Actualizar los estudios recientes']"));
    spanish.click(spanish.$$(".side-btn").find((b) => b.dataset.tip === "Configuración")); await until(() => spanish.$("form.settings-form"), 6000);
    check("...Settings: the tabs, and the portal's language in Appearance", spanish.$$(".subtabs [role=tab]").map((b) => b.textContent).slice(0, 4).join() === "General,Apariencia,Búsqueda y etiquetas,Funciones"
      && spanish.$$("section.settings-section h2").some((h) => h.textContent === "Idioma, fechas, nombres y estilo"), spanish.$$(".subtabs [role=tab]").map((b) => b.textContent).join());
    const back = spanish.$(".simple-head select.language-select");
    back.value = "en"; back.dispatchEvent(new spanish.window.Event("change"));
    await until(() => spanish.window.document.documentElement.lang === "en", 6000); await sleep(300);
    check("...and back to English", spanish.$$(".subtabs [role=tab]").map((b) => b.textContent).slice(0, 2).join() === "General,Appearance");

    await admin.call("PUT", "/api/admin/settings", { language: "es" });
    const login = await makeDom(STAFF); await until(() => login.$(".login-card"), 6000); await sleep(300);
    check("the portal's language (Settings > Appearance): the login page is in Spanish", /Iniciar sesión/.test(login.text()) && login.window.document.documentElement.lang === "es", login.text().slice(0, 120));
    await login.login("nobody-here", "wrong-password-1");
    check("...and so are the server's messages", /Usuario o contraseña incorrectos/.test(login.$(".form-error").textContent), login.$(".form-error").textContent);
    const link = await newLink(admin, { minutes: 90, downloads: 2 });
    const guestEs = await guestOpens(link.token, link.password);
    check("...and the invitation page", /Estudio compartido/.test(guestEs.text()) && /Descargas restantes/.test(guestEs.text()) && guestEs.window.document.documentElement.lang === "es", guestEs.text().slice(0, 160));
    const guestMenu = guestEs.$(".share-head select.language-select");
    guestMenu.value = "en"; guestMenu.dispatchEvent(new guestEs.window.Event("change"));
    await until(() => /Shared study/.test(guestEs.text()), 6000);
    check("...where a visitor can choose another language", /Shared study/.test(guestEs.text()) && /Downloads left/.test(guestEs.text()));
    await admin.call("PUT", "/api/admin/settings", { language: "en" });
    check("no JavaScript errors in the translated pages", [spanish, login, guestEs].every((d) => d.errors.length === 0), [spanish, login, guestEs].flatMap((d) => d.errors).join("|"));
    spanish.window.close(); login.window.close(); guestEs.window.close();
  }

  console.log("--- search keeps working (modality filter)");
  await admin.tab("Studies");
  admin.click(admin.$$(".mod-picker .chip-toggle").find((b) => b.textContent === "RX")); await sleep(1000);
  const lastFind = fs.readFileSync(path.join(LOGS, "find.log"), "utf8").trim().split("\n").pop();
  check("RX chip searches CR + DX", /"ModalitiesInStudy": "CR\\\\DX"/.test(lastFind), lastFind);

  console.log("--- two-step sign-in, My account, statistics, the Share options, upload links");
  const totp = (secret) => {
    const clean = secret.replace(/\s/g, "");
    let bits = "", bytes = [];
    for (const c of clean) bits += "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".indexOf(c).toString(2).padStart(5, "0");
    for (let i = 0; i + 8 <= bits.length; i += 8) bytes.push(parseInt(bits.slice(i, i + 8), 2));
    const counter = Buffer.alloc(8); counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000)));
    const mac = require("crypto").createHmac("sha1", Buffer.from(bytes)).update(counter).digest();
    const o = mac[19] & 15;
    return String((mac.readUInt32BE(o) & 0x7fffffff) % 1000000).padStart(6, "0");
  };
  await admin.call("POST", "/api/admin/users", { username: "twostep", password: "TwoStepPass123", permissions: ["read"] });
  await admin.call("PUT", "/api/admin/settings", { two_factor: "required", upload_links: true });
  const ts = await makeDom(STAFF);
  await ts.login("twostep", "TwoStepPass123");
  check("required two-step sign-in: the set-up comes after the password", !!ts.$(".two-factor-step img.qr") && !ts.$(".simple-side"), ts.text().slice(0, 160));
  const key = ts.$(".totp-setup .password").textContent;
  ts.$(".two-factor-step input[name=code]").value = totp(key);
  ts.submit(ts.$(".two-factor-step")); await until(() => ts.$(".simple-side"), 6000);
  check("...the first code signs in", !!ts.$(".simple-side"));
  check("...and the recovery codes are shown once", ts.$$("dialog[open] .recovery-codes li").length === 10);
  ts.click(ts.button(ts.$("dialog[open]"), "I have saved them"));
  await ts.tab("My account");
  check("My account: email, password and two-step sign-in", /Email address/.test(ts.text()) && /Change password/.test(ts.text()) && /Recovery codes left: 10/.test(ts.text()), ts.text().slice(0, 200));
  check("...required: it cannot be switched off", !ts.button(ts.$("main"), "Switch it off"));
  ts.$("main input[type=email]").value = "two@example.org";
  ts.submit(ts.$("main input[type=email]").closest("form")); await sleep(1000);
  check("...the email is saved", (await (await ts.call("GET", "/api/me")).json()).email === "two@example.org");
  await admin.call("PUT", "/api/admin/settings", { two_factor: "off" });

  const st = await makeDom(STAFF); await st.login("admin", "SuperSecret123");
  await st.tab("Settings");
  st.click(st.$$(".subtabs [role=tab]").find((b) => b.textContent === "Statistics")); await until(() => st.$(".stat-tile"), 8000);
  check("Statistics: the tiles, a chart per series, the table by month", st.$$(".stat-tile").length === 6 && st.$$("svg.month-bars").length === 6 && st.$$(".stats-table thead th").length === 13);
  st.click(st.$$(".subtabs [role=tab]").find((b) => b.textContent === "Auto-forward")); await sleep(300);
  st.click(st.button(st.section("Auto-forward"), "Add rule"));
  check("Auto-forward: a rule row with the destinations of Orthanc", st.$$(".forward-rule select option").some((o) => o.value === "ris"));
  await st.tab("Studies"); await until(() => st.$("tr.study"), 6000);
  st.click(st.button(st.$("tr.study"), "Share..."));
  const mailOn = (await (await st.call("GET", "/api/me")).json()).features.share.notify;
  check("Download menu: Patient CD / USB", st.$$("tr.study .menu-item").some((a) => a.textContent.startsWith("Patient CD / USB")));
  const direct = st.$("dialog[open]");
  check("Share window of a study: short, the less common options folded under More options",
    !/Every study of/.test(direct.querySelector("form").textContent.replace(/To share every study[^.]*\./, ""))
      && !!direct.querySelector("details.more-options:not([open])") && /Share an anonymized copy/.test(direct.querySelector("details.more-options").textContent)
      && /Email me when the link is opened/.test(direct.textContent) === mailOn,
    direct.textContent.slice(0, 300));
  check("...and it says where the advanced sharing is", /New share\.\.\." on the Shares page/.test(direct.textContent));
  direct.querySelector("button[type=button]").click();
  await st.tab("Shares");
  const sharesBefore = (await (await st.call("GET", "/api/shares")).json()).length;
  st.click(st.button(st.$("main"), "New share..."));
  const adv = st.$("dialog.advanced-share[open]");
  check("Shares > New share: every study of a patient, or of a label", !!adv && /Every study of a patient/.test(adv.textContent) && /Every study with a label/.test(adv.textContent));
  adv.querySelector("input[name=patient_id]").value = "P-001";
  st.click(st.button(adv, "Find")); await until(() => /studies will be shared/.test(adv.textContent), 6000);
  check("...it shows exactly which studies before the link is made", /studies will be shared/.test(adv.textContent) && adv.querySelectorAll(".share-preview li").length >= 1);
  st.submit(adv.querySelector("form")); await sleep(300);
  const second = st.$("dialog.share-dialog[open]");
  check("...then the Share window, with the options shown", !!second && /Share every study of/.test(second.textContent) && !second.querySelector("details.more-options"));
  st.submit(second.querySelector("form")); await until(() => /Link and password created/.test(second.textContent), 6000);
  check("...and the link is created", (await (await st.call("GET", "/api/shares")).json()).length === sharesBefore + 1);
  second.querySelector(".dialog-actions .btn:last-child").click();
  check("Shares: a tab for upload links", st.$$(".subtabs [role=tab]").some((b) => /Upload links/.test(b.textContent)));

  const ul = await (await admin.call("POST", "/api/upload-links", { note: "ui test" })).json();
  const sender = await makeDom(GUEST, `/u/${ul.url.split("/u/")[1]}`, SHARE);
  await until(() => sender.$("#upload-password"), 6000);
  check("upload link page: the password first", !!sender.$("#upload-password") && /Send us your images/.test(sender.text()));
  sender.$("#upload-password").value = ul.password;
  sender.submit(sender.$("form.share-password")); await until(() => sender.$(".dropzone"), 6000);
  check("...then the area to drop files, and nothing of the archive", !!sender.$(".dropzone") && !/DOE/.test(sender.text()), sender.text().slice(0, 200));
  await admin.call("PUT", "/api/admin/settings", { upload_links: false });

  console.log("--- colours of modality buttons and labels");
  {
    const cur = (await (await admin.call("GET", "/api/admin/settings")).json()).settings;
    const buttons = cur.modality_buttons.map((b) => (b.label === "CT" ? { ...b, color: "#d9480f" } : b));
    await admin.call("PUT", "/api/admin/settings", { modality_buttons: buttons, label_colors: { "second-opinion": "#2b8a3e" } });
    const c = await makeDom(STAFF); await c.login("admin", "SuperSecret123");
    await c.tab("Studies"); await until(() => c.$("tr.study"), 6000);
    const ctButton = c.$$(".mod-picker .chip-toggle").find((b) => b.textContent === "CT");
    check("the CT filter button wears its colour", ctButton.classList.contains("tinted") && ctButton.style.getPropertyValue("--chip") === "#d9480f");
    const ctTag = c.$$("tr.study .chip[data-m]").find((el) => el.dataset.m === "CT");
    check("...and so do the CT tags of the table", !!ctTag && ctTag.style.getPropertyValue("--chip") === "#d9480f");
    const srTag = c.$$("tr.study .chip[data-m]").find((el) => el.dataset.m === "SR");
    check("...the others keep the built-in colours", !!srTag && !srTag.classList.contains("tinted"));
    const labelToggle = c.$$(".label-toggle").find((b) => b.textContent.startsWith("second-opinion"));
    check("a label's filter button wears its colour", !!labelToggle && labelToggle.style.getPropertyValue("--chip") === "#2b8a3e");
    await c.tab("Settings");
    c.click(c.$$(".subtabs [role=tab]").find((b) => b.textContent === "Search & labels")); await sleep(1500);
    check("Settings: a colour column in the modality buttons, and the label colours", !!c.$(".modbtn-head") && /Colour/.test(c.$(".modbtn-head").textContent)
      && c.$$(".modbtn-row input[type=color]").length >= 1 && c.$$(".label-color-row").length >= 1);
    await admin.call("PUT", "/api/admin/settings", { modality_buttons: cur.modality_buttons, label_colors: {} });
    all_extra.push(c);
  }

  const all = [...all_extra, admin, guest, revokedPage, ts, st, sender, viewOnlyPage, shortPage, expiredPage, nothing, garbage, unknown, noViewerPage, adminNoOhif, viewers, noButtons, sharer, viewer, reader, linked, viaUid, live, custom];
  check("no JavaScript errors anywhere", all.every((d) => d.errors.length === 0), all.flatMap((d) => d.errors).join("|"));
  console.log(`\nRESULT: ${results.filter(Boolean).length} passed, ${results.filter((r) => !r).length} failed`);
  process.exit(results.every(Boolean) ? 0 : 1);
})().catch((e) => { console.error("HARNESS ERROR", e); process.exit(2); });
