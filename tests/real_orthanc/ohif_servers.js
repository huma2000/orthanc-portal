// Real OHIF through the portal connected to two real Orthancs (phase 2: the portal itself picks
// the server). Usage: node ohif_servers.js PORTAL_URL ADMIN_COOKIE ONLYB_COOKIE OUT_DIR
// Study 101 is only on A (main), 102 only on B (archive), 103 on both (A: squares, B: rings).
const puppeteer = require("puppeteer-core");
const fs = require("fs");
const [base, admin, onlyb, out] = process.argv.slice(2);
const R = "1.2.826.0.1.3680043.9.7777.";
const cases = [
  { name: "admin-101-only-on-A", who: admin, uid: R + "101" },
  { name: "admin-102-only-on-B-found", who: admin, uid: R + "102" },
  { name: "admin-103-opened-from-archive", who: admin, uid: R + "103", server: "archive" },
  { name: "admin-103-opened-from-main", who: admin, uid: R + "103", server: "main" },
  { name: "onlyb-103", who: onlyb, uid: R + "103" },
  { name: "onlyb-103-asks-main-ignored", who: onlyb, uid: R + "103", server: "main" },
  { name: "onlyb-101-not-on-its-server", who: onlyb, uid: R + "101" },
];

(async () => {
  const browser = await puppeteer.connect({ browserURL: "http://127.0.0.1:29222" });
  const summary = [];
  for (const c of cases) {
    const page = await browser.newPage();
    await page.setViewport({ width: 1280, height: 900 });
    await page.setCookie({ name: "portal_session", value: c.who, url: base });
    const requests = [], errors = [];
    page.on("response", (r) => {
      const u = new URL(r.url());
      if (u.pathname.startsWith("/dicom-web/") || u.pathname === "/wado")
        requests.push(`${r.status()} ${u.pathname}${u.search}`.slice(0, 160));
    });
    page.on("console", (m) => { if (m.type() === "error") errors.push(m.text().slice(0, 200)); });
    page.on("pageerror", (e) => errors.push("pageerror: " + String(e).slice(0, 200)));
    const url = `${base}/ohif/viewer?StudyInstanceUIDs=${c.uid}${c.server ? `&server=${c.server}` : ""}`;
    await page.goto(url, { waitUntil: "networkidle0", timeout: 90000 }).catch((e) => errors.push("goto: " + e.message));
    await new Promise((r) => setTimeout(r, 6000));
    const drawn = await page.evaluate(() => {
      const cs = [...document.querySelectorAll("canvas")].sort((a, b) => b.width * b.height - a.width * a.height);
      if (!cs.length) return "no canvas";
      const cv = cs[0];
      try {
        const g = cv.getContext("2d");
        if (!g) return `canvas ${cv.width}x${cv.height} (not 2d)`;
        const d = g.getImageData(0, 0, cv.width, cv.height).data;
        let lit = 0;
        for (let i = 0; i < d.length; i += 16) if (d[i] + d[i + 1] + d[i + 2] > 30) lit++;
        return `lit samples ${lit}`;
      } catch (e) { return "canvas read error " + e.message; }
    });
    await page.screenshot({ path: `${out}/${c.name}.png` });
    const failed = requests.filter((r) => !/^(200|204|206|304) /.test(r));
    summary.push({ case: c.name, requests: requests.length, failed: failed.slice(0, 4), drawn, errors: errors.filter((e) => !/manifest/.test(e)).slice(0, 3) });
    fs.writeFileSync(`${out}/${c.name}.requests.txt`, requests.join("\n"));
    await page.close();
  }
  console.log(JSON.stringify(summary, null, 1));
  await browser.disconnect();
})();
