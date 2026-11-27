// Opens real OHIF through the portal for each test study and records every DICOMweb / WADO
// request the browser makes, whether the image was drawn, and a screenshot. See README.md.
//
//   node ohif_check.js PORTAL_URL SESSION_COOKIE OUT_DIR [DEVTOOLS_URL] [ROUTER_URL]
//
// The browser is an Edge (or Chrome) already started headless with --remote-debugging-port
// (default http://127.0.0.1:29222); it is left running. ROUTER_URL (default
// http://127.0.0.1:28050) is uid_router.py, used to "pin" study 103 to one server or the other.
const puppeteer = require("puppeteer-core");
const fs = require("fs");
const [base, cookie, out, devtools = "http://127.0.0.1:29222", router = "http://127.0.0.1:28050"] =
  process.argv.slice(2);
const R = "1.2.826.0.1.3680043.9.7777.";
const cases = [
  { name: "s101-only-on-A", uid: R + "101" },
  { name: "s102-only-on-B", uid: R + "102" },
  { name: "s103-both-pinned-B", uid: R + "103", pin: "B" },
  { name: "s103-both-pinned-A", uid: R + "103", pin: "A" },
];

(async () => {
  const browser = await puppeteer.connect({ browserURL: devtools });
  const summary = [];
  for (const c of cases) {
    if (c.pin) await fetch(`${router}/__pin?uid=${c.uid}&server=${c.pin}`, { method: "POST" });
    const page = await browser.newPage();
    await page.setViewport({ width: 1280, height: 900 });
    await page.setCookie({ name: "portal_session", value: cookie, url: base });
    const requests = [], errors = [];
    page.on("response", (r) => {
      const u = new URL(r.url());
      if (u.pathname.startsWith("/dicom-web/") || u.pathname === "/wado")
        requests.push(`${r.status()} ${u.pathname}${u.search}`.slice(0, 160));
    });
    page.on("console", (m) => { if (m.type() === "error") errors.push(m.text().slice(0, 200)); });
    page.on("pageerror", (e) => errors.push("pageerror: " + String(e).slice(0, 200)));
    await page
      .goto(`${base}/ohif/viewer?StudyInstanceUIDs=${c.uid}`, { waitUntil: "networkidle0", timeout: 90000 })
      .catch((e) => errors.push("goto: " + e.message));
    await new Promise((r) => setTimeout(r, 6000));
    const drawn = await page.evaluate(() => {
      const cs = [...document.querySelectorAll("canvas")].sort((a, b) => b.width * b.height - a.width * a.height);
      if (!cs.length) return "no canvas";
      const g = cs[0].getContext("2d");
      if (!g) return "canvas (not 2d)";
      const d = g.getImageData(0, 0, cs[0].width, cs[0].height).data;
      let lit = 0;
      for (let i = 0; i < d.length; i += 16) if (d[i] + d[i + 1] + d[i + 2] > 30) lit++;
      return `canvas ${cs[0].width}x${cs[0].height}, lit samples ${lit}`;
    });
    await page.screenshot({ path: `${out}/${c.name}.png` });
    const failed = requests.filter((r) => !/^(200|206|304) /.test(r));
    summary.push({ case: c.name, requests: requests.length, failed, drawn, errors: errors.slice(0, 5) });
    fs.writeFileSync(`${out}/${c.name}.requests.txt`, requests.join("\n"));
    await page.close();
  }
  console.log(JSON.stringify(summary, null, 1));
  await browser.disconnect();
})();
