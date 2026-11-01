// Browser-side tests: the portal's UI (static/app.js) driven in jsdom against the live portal + fake Orthanc.
const { JSDOM } = require("jsdom");
const fs = require("fs");
const path = require("path");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const check = (name, ok, extra = "") => { results.push(ok); console.log(`${ok ? "PASS" : "FAIL"}  ${name} ${ok ? "" : extra}`); };
const APP = fs.readFileSync(path.join(__dirname, "..", "static", "app.js"), "utf8");
const LOGS = process.env.MOCK_LOG_DIR || "/tmp/portal-tests";
const FULL = "http://127.0.0.1:18080", REMOTE = "http://127.0.0.1:18081";

async function makeDom(base) {
  let cookie = "";
  const html = (await (await fetch(base + "/")).text()).replace(/<script src[^>]*><\/script>/, "");
  const dom = new JSDOM(html, { url: base + "/", runScripts: "outside-only", pretendToBeVisual: true });
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
  window.eval(APP);
  const t = {
    window, errors, call: (method, url, body) => window.fetch(url, { method, headers: { "X-Requested-With": "portal", "Content-Type": "application/json" }, body: body && JSON.stringify(body) }),
    $: (s) => window.document.querySelector(s),
    $$: (s) => Array.from(window.document.querySelectorAll(s)),
    click: (el) => el.dispatchEvent(new window.MouseEvent("click", { bubbles: true })),
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
  };
  return t;
}

(async () => {
  console.log("--- intranet administrator: both viewers");
  const admin = await makeDom(FULL);
  await admin.login("admin", "SuperSecret123");
  await sleep(1200);
  check("all tabs are there", admin.tabs().join() === "Studies,Series,Upload,Transfers,Users,DICOM nodes,Settings,Audit log", admin.tabs().join());
  const row = admin.$("tr.study");
  check("study row: preview, modalities, patient", !!row.querySelector(".thumb") && /DOE, JOHN/.test(row.textContent) && /CT/.test(row.textContent));
  check("View menu offers Weasis (a desktop viewer)", admin.$$("details.menu button.menu-item").some((b) => /Weasis/.test(b.textContent)));
  const ohifItem = admin.$$("details.menu a.menu-item").find((a) => /\/ohif\/viewer\?StudyInstanceUIDs=/.test(a.getAttribute("href")));
  check("...and the browser viewer (opens a new tab)", !!ohifItem && ohifItem.getAttribute("target") === "_blank" && ohifItem.getAttribute("rel") === "noopener");
  check("the hint mentions installing Weasis", /Weasis needs to be installed/.test(admin.$(".page-head").textContent));
  check("Send, Download and View controls on the intranet", /Send to/.test(row.textContent) && /Download/.test(row.textContent) && /View/.test(row.textContent));

  console.log("--- internet administrator: the browser viewer ONLY");
  const remote = await makeDom(REMOTE);
  await remote.login("admin", "SuperSecret123");
  await sleep(1200);
  check("only Studies + Series tabs and the remote badge", remote.tabs().join() === "Studies,Series" && !!remote.$(".badge-remote"), remote.tabs().join());
  const rrow = remote.$("tr.study");
  const link = rrow.querySelector("a.view-link");
  check("one plain 'View' button that opens the browser viewer in a new tab", !!link && link.textContent === "View" && /^\/ohif\/viewer\?StudyInstanceUIDs=1\.2\.826/.test(link.getAttribute("href")) && link.getAttribute("target") === "_blank" && link.getAttribute("rel") === "noopener", rrow.innerHTML.slice(0, 200));
  check("no View drop-down menu, no Weasis anywhere on the page", !remote.$("details.menu") && !/weasis/i.test(remote.window.document.body.textContent), remote.window.document.body.textContent.match(/.{20}weasis.{20}/i));
  check("the hint says images open in the browser and are not downloaded", /browser viewer/.test(remote.$(".page-head").textContent) && /not downloaded/.test(remote.$(".page-head").textContent), remote.$(".page-head").textContent);
  check("no Send / Download over the internet", !/Send to/.test(rrow.textContent) && !/Download/.test(rrow.textContent));
  remote.click(remote.$(".twisty")); await sleep(1500);
  check("series list still works remotely (with previews)", remote.$$(".series-box tbody tr").length === 2 && !!remote.$(".series-box .thumb"));
  await remote.tab("Series");
  check("Series tab: 'View' link too", !!remote.$("tr.study a.view-link") && !remote.$("tr.study details.menu"));
  check("the Weasis link API is refused for this session (403 or missing)", [403, 404].includes((await remote.call("GET", "/api/weasis/link?study_uid=1.2.3")).status));

  console.log("--- internet without the browser viewer installed");
  await admin.call("PUT", "/api/admin/settings", { ohif: "off" });
  const noOhif = await makeDom(REMOTE);
  await noOhif.login("admin", "SuperSecret123");
  await sleep(1000);
  check("no View control at all (there is nothing it could open)", !noOhif.$("a.view-link") && !noOhif.$("details.menu") && noOhif.$$("tr.study").length === 1);
  check("the page explains why", /not available on this server/.test(noOhif.$(".page-head").textContent), noOhif.$(".page-head").textContent);
  const adminNoOhif = await makeDom(FULL);
  await adminNoOhif.login("admin", "SuperSecret123");
  await sleep(1000);
  check("on the intranet Weasis is still there when OHIF is off", [...adminNoOhif.$$("tr.study button")].some((b) => /Weasis/.test(b.textContent)));
  await admin.call("PUT", "/api/admin/settings", { ohif: "auto" });

  console.log("--- Settings > Network: the new switch");
  await admin.tab("Settings");
  const net = admin.section("Network");
  const sw = admin.labelInput(net, "Internet viewers cannot download whole DICOM files");
  check("switch is there and ON by default", sw.type === "checkbox" && sw.checked === true);
  check("its explanation mentions Weasis and the viewer", /Weasis/.test(net.textContent) && /metadata and pixel frames/.test(net.textContent));
  sw.checked = false;
  admin.submit(admin.$("form.settings-form")); await sleep(1500);
  check("saved (and reflected by the server)", (await (await admin.call("GET", "/api/admin/settings")).json()).settings.remote_block_dicom_files === false);
  const net2 = admin.section("Network");
  admin.labelInput(net2, "Internet viewers cannot download whole DICOM files").checked = true;
  admin.submit(admin.$("form.settings-form")); await sleep(1500);
  check("switched back on", (await (await admin.call("GET", "/api/admin/settings")).json()).settings.remote_block_dicom_files === true);

  console.log("--- a viewer account on the internet");
  await admin.call("POST", "/api/admin/users", { username: "viewer1", password: "ViewerPass123", permissions: ["read", "visualize"] });
  const viewer = await makeDom(REMOTE);
  await viewer.login("viewer1", "ViewerPass123");
  await sleep(1200);
  check("viewer: Studies/Series, View link, nothing else", viewer.tabs().join() === "Studies,Series" && !!viewer.$("tr.study a.view-link"));
  await admin.call("POST", "/api/admin/users", { username: "reader", password: "ReaderPass123", permissions: ["read"] });
  const reader = await makeDom(REMOTE);
  await reader.login("reader", "ReaderPass123");
  await sleep(1000);
  check("read-only account: the list, no View, no previews, no hint", reader.$$("tr.study").length === 1 && !reader.$("a.view-link") && !reader.$(".thumb"));

  console.log("--- search keeps working (modality filter)");
  admin.click(admin.$$(".nav button").find((b) => b.textContent === "Studies")); await sleep(1000);
  admin.click(admin.$$(".chip-toggle").find((b) => b.textContent === "RX")); await sleep(1000);
  const lastFind = fs.readFileSync(path.join(LOGS, "find.log"), "utf8").trim().split("\n").pop();
  check("RX chip searches CR + DX", /"ModalitiesInStudy": "CR\\\\DX"/.test(lastFind), lastFind);

  check("no JavaScript errors anywhere", [admin, remote, noOhif, adminNoOhif, viewer, reader].every((d) => d.errors.length === 0), [admin, remote, noOhif, adminNoOhif, viewer, reader].flatMap((d) => d.errors).join("|"));
  console.log(`\nRESULT: ${results.filter(Boolean).length} passed, ${results.filter((r) => !r).length} failed`);
  process.exit(results.every(Boolean) ? 0 : 1);
})().catch((e) => { console.error("HARNESS ERROR", e); process.exit(2); });
