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
const sql = (statement) => execFileSync("python3", ["-c", "import sqlite3,sys;c=sqlite3.connect(sys.argv[1],timeout=10);c.execute(sys.argv[2]);c.commit()", path.join(LOGS, "portal.db"), statement]);
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
    tabs: () => t.$$(".nav button").map((b) => b.textContent),
    tab: async (name) => { t.click(t.$$(".nav button").find((b) => b.textContent === name)); await sleep(1200); },
    section: (title) => t.$$("section.settings-section").find((s) => s.querySelector("h2").textContent === title),
    labelInput: (scope, text) => Array.from(scope.querySelectorAll("label")).find((l) => l.textContent.startsWith(text)).querySelector("input,select,textarea"),
    button: (scope, text) => Array.from(scope.querySelectorAll("button")).find((b) => b.textContent === text),
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
  console.log("--- intranet administrator: the staff portal");
  const admin = await makeDom(STAFF);
  await admin.login("admin", "SuperSecret123");
  await sleep(1200);
  check("all tabs are there, including Shares", admin.tabs().join() === "Studies,Series,Upload,Transfers,Shares,Settings", admin.tabs().join());

  console.log("--- Settings is organised in tabs (users, DICOM nodes and the audit log live there)");
  await admin.tab("Settings");
  await until(() => admin.$("form.settings-form"), 6000);
  const subtab = (name) => admin.$$(".subtabs [role=tab]").find((b) => b.textContent === name);
  const visibleSections = () => admin.$$("section.settings-section").filter((s) => !s.hidden).map((s) => s.querySelector("h2").textContent);
  check("Settings tabs", admin.$$(".subtabs [role=tab]").map((b) => b.textContent).join() === "General,Orthanc,Network & sharing,Security & roles,Users,DICOM nodes,Audit log",
    admin.$$(".subtabs [role=tab]").map((b) => b.textContent).join());
  check("no stray 'null' text on the page (it used to appear under the title)",
    !admin.$$("main *").some((el) => Array.from(el.childNodes).some((n) => n.nodeType === 3 && n.textContent.trim() === "null")));
  check("General shows Branding, Modality buttons and Viewers", visibleSections().join() === "Branding,Modality buttons,Viewers and uploads", visibleSections().join());
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
  const ohifButton = Array.from(row.querySelectorAll("a.btn")).find((a) => a.textContent === "OHIF");
  check("...and a separate OHIF button (browser viewer, new tab)", !!ohifButton && /\/ohif\/viewer\?StudyInstanceUIDs=/.test(ohifButton.getAttribute("href"))
    && ohifButton.getAttribute("target") === "_blank" && ohifButton.getAttribute("rel") === "noopener");
  check("the Studies page uses the whole width (wide layout)", admin.$("main").classList.contains("wide"));
  check("no accession number: neither a search box nor a column",
    !admin.$("input[name=accession_number]") && !admin.$$("main th").some((th) => /Accession/.test(th.textContent)) && !/ACC1/.test(row.textContent));
  const tagTitle = (code) => { const tag = Array.from(row.querySelectorAll(".chip")).find((c) => c.textContent === code); return tag && tag.getAttribute("title"); };
  check("modality tags explain themselves on hover: CT (button description), SR (DICOM name)",
    tagTitle("CT") === "CT: Computed tomography" && tagTitle("SR") === "SR: Structured report", `${tagTitle("CT")} | ${tagTitle("SR")}`);
  check("the default modality buttons, in the default order", admin.$$(".chip-toggle").map((b) => b.textContent).join() === "CT,MR,US,RX,MG,XA,RF,NM,PT", admin.$$(".chip-toggle").map((b) => b.textContent).join());
  check("the study date never wraps", row.querySelectorAll("td.nowrap").length > 0);
  check("the old View drop-down is gone", !Array.from(row.querySelectorAll("summary")).some((s) => s.textContent === "View"));
  check("the Weasis notice is no longer on the Studies page", !/Weasis needs to be/.test(admin.$("main").textContent));
  check("Send, Share, Download, Weasis and OHIF controls on every study", /Send to/.test(row.textContent) && /Share\.\.\./.test(row.textContent) && /Download/.test(row.textContent) && /Weasis/.test(row.textContent) && /OHIF/.test(row.textContent));
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
  const [hoursInput, downloadsInput] = dialog.querySelectorAll("input[type=number]");
  check("the form is ready at once after saving the address, with the defaults 48 hours and 1 download", !!hoursInput && hoursInput.value === "48" && downloadsInput.value === "1", dialog.textContent);
  check("the form names the study", /DOE, JOHN/.test(dialog.textContent));
  dialog.querySelector("input[maxlength]").value = "Dr. Who";
  admin.submit(dialog.querySelector("form"));
  await until(() => dialog.querySelector(".password"));
  const url = dialog.querySelector(".secret-box input").value, password = dialog.querySelector(".password").textContent;
  const token = url.split("/s/")[1];
  check("the link is <public address>/s/<code>", /^https:\/\/pacs\.example\.com\/s\/[0-9a-f]{48}$/.test(url), url);
  check("the password has 3 groups of 4 readable characters", /^[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}$/.test(password), password);
  check("copy buttons for the link, the password and the invitation text", ["Copy link", "Copy password", "Copy invitation text (without the password)"].every((n) => !!admin.button(dialog, n)));
  check("the dialog warns that the password cannot be shown again", /shown only now/.test(dialog.textContent));
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
  check("the page has no staff controls (no tabs, no search)", !guest.$(".nav") && !guest.$(".chip-toggle") && !/weasis/i.test(guest.text()));
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
  admin.click(admin.$$(".nav button").find((b) => b.textContent === "Studies")); await sleep(300);
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
  check("on the intranet Weasis is still there when OHIF is off", [...adminNoOhif.$$("tr.study button")].some((b) => /Weasis/.test(b.textContent)));
  check("...and the OHIF button is gone", !adminNoOhif.$$("tr.study a.btn").some((a) => a.textContent === "OHIF"));
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
    !viewers.button(viewers.$("tr.study"), "Weasis") && viewers.$$("tr.study a.btn").some((a) => a.textContent === "OHIF"));
  await admin.call("PUT", "/api/admin/settings", { weasis_enabled: true });

  console.log("--- modality buttons: order and visibility chosen in Settings");
  await viewers.tab("Settings");
  await until(() => viewers.section("Modality buttons"), 6000);
  const modSection = viewers.section("Modality buttons");
  const modRows = () => Array.from(viewers.section("Modality buttons").querySelectorAll(".modbtn-row"));
  const rowOf = (label) => modRows().find((r) => r.querySelector(".modbtn-label").value === label);
  const previewChips = () => Array.from(viewers.section("Modality buttons").querySelectorAll(".modbtn-preview .chip-toggle")).map((b) => b.textContent).join();
  check("Settings > General has the modality buttons editor, with the 9 defaults", !modSection.hidden && modRows().length === 9 && previewChips() === "CT,MR,US,RX,MG,XA,RF,NM,PT", previewChips());
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
  check("the Studies page follows at once: RX first, no CT", viewers.$$(".chip-toggle").map((b) => b.textContent).join() === "RX,MR,US,MG,XA,RF,NM,PT", viewers.$$(".chip-toggle").map((b) => b.textContent).join());
  await viewers.tab("Series"); await until(() => viewers.$(".mod-picker"), 6000);
  check("...and so does the Series page", viewers.$$(".chip-toggle").map((b) => b.textContent).join() === "RX,MR,US,MG,XA,RF,NM,PT");
  await admin.call("PUT", "/api/admin/settings", { modality_buttons: savedButtons.map((b) => ({ ...b, visible: false })) });
  const noButtons = await makeDom(STAFF);
  await noButtons.login("admin", "SuperSecret123");
  await until(() => noButtons.$(".mod-picker"), 6000);
  check("with every button hidden, the Other codes box is still there", noButtons.$$(".chip-toggle").length === 0 && !!noButtons.$(".mod-picker .mod-other"));
  const defaultButtons = (await (await admin.call("GET", "/api/admin/settings")).json()).defaults.modality_buttons;
  await admin.call("PUT", "/api/admin/settings", { modality_buttons: defaultButtons });

  console.log("--- staff accounts: who sees what");
  await admin.call("POST", "/api/admin/users", { username: "sharer", password: "SharerPass123", permissions: ["read", "visualize", "share"] });
  const sharer = await makeDom(STAFF);
  await sharer.login("sharer", "SharerPass123");
  await sleep(1200);
  check("sharer: Studies, Series and Shares only", sharer.tabs().join() === "Studies,Series,Shares", sharer.tabs().join());
  const srow = sharer.$("tr.study");
  check("sharer: Share... and the viewers, but no Send or Download", /Share\.\.\./.test(srow.textContent) && /Weasis/.test(srow.textContent) && /OHIF/.test(srow.textContent) && !/Send to/.test(srow.textContent) && !/Download/.test(srow.textContent), srow.textContent);
  await sharer.tab("Shares");
  check("sharer: sees none of the administrators' links", /No invitation yet/.test(sharer.text()), sharer.text().slice(-200));
  await admin.call("POST", "/api/admin/users", { username: "viewer1", password: "ViewerPass123", permissions: ["read", "visualize"] });
  const viewer = await makeDom(STAFF);
  await viewer.login("viewer1", "ViewerPass123");
  await sleep(1200);
  check("viewer: Studies and Series, no Share button, no Shares tab", viewer.tabs().join() === "Studies,Series" && !/Share\.\.\./.test(viewer.$("tr.study").textContent));
  await admin.call("POST", "/api/admin/users", { username: "reader", password: "ReaderPass123", permissions: ["read"] });
  const reader = await makeDom(STAFF);
  await reader.login("reader", "ReaderPass123");
  await sleep(1000);
  check("read-only account: the list, no View, no Share, no previews", reader.$$("tr.study").length === 1 && !reader.$("details.menu") && !/Share\.\.\./.test(reader.$("tr.study").textContent) && !reader.$(".thumb"));

  console.log("--- search keeps working (modality filter)");
  admin.click(admin.$$(".nav button").find((b) => b.textContent === "Studies")); await sleep(1000);
  admin.click(admin.$$(".chip-toggle").find((b) => b.textContent === "RX")); await sleep(1000);
  const lastFind = fs.readFileSync(path.join(LOGS, "find.log"), "utf8").trim().split("\n").pop();
  check("RX chip searches CR + DX", /"ModalitiesInStudy": "CR\\\\DX"/.test(lastFind), lastFind);

  const all = [admin, guest, revokedPage, viewOnlyPage, shortPage, expiredPage, nothing, garbage, unknown, noViewerPage, adminNoOhif, viewers, noButtons, sharer, viewer, reader];
  check("no JavaScript errors anywhere", all.every((d) => d.errors.length === 0), all.flatMap((d) => d.errors).join("|"));
  console.log(`\nRESULT: ${results.filter(Boolean).length} passed, ${results.filter((r) => !r).length} failed`);
  process.exit(results.every(Boolean) ? 0 : 1);
})().catch((e) => { console.error("HARNESS ERROR", e); process.exit(2); });
