/* CDP probe 2 (self-managing): spawn chrome, probe stats view, screenshot, kill chrome */
const { spawn } = require("child_process");
const fs = require("fs");

const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const URL = process.argv[2] || "http://127.0.0.1:8799/webui/#/stats";
const SHOT = process.argv[3] || "D:/Projects/cc-proxy/.probe/cdp-stats-1440.png";
const W = Number(process.argv[4] || 1440), H = Number(process.argv[5] || 900);
const debugPort = 9333;

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox",
    `--remote-debugging-port=${debugPort}`,
    `--window-size=${W},${H}`,
    "--user-data-dir=/tmp/cdp-profile-3",
    "about:blank",
  ], { stdio: "ignore" });
  const cleanup = () => { try { chrome.kill(); } catch {} };
  process.on("exit", cleanup);

  try {
    let target = null;
    for (let i = 0; i < 30; i++) {
      try {
        const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
        target = list.find((t) => t.type === "page");
        if (target) break;
      } catch {}
      await new Promise((r) => setTimeout(r, 500));
    }
    if (!target) throw new Error("no page target");
    const ws = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
    let id = 0;
    const send = (method, params) => new Promise((res, rej) => {
      const mid = ++id;
      ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id === mid) { m.error ? rej(new Error(m.error.message)) : res(m.result); } };
      ws.send(JSON.stringify({ id: mid, method, params }));
    });
    await send("Runtime.enable", {});
    await send("Page.enable", {});
    await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: false });
    await send("Page.navigate", { url: URL });
    await new Promise((r) => setTimeout(r, 3000));

    const expr = `(async () => {
      const out = {};
      const svg = document.getElementById("trend-chart");
      out.svgViewBox = svg.getAttribute("viewBox");
      out.yLabels = [...svg.querySelectorAll("text")].map(t => t.textContent);
      out.paths = [...svg.querySelectorAll("path")].map(p => {
        const d = p.getAttribute("d") || "";
        return { stroke: p.getAttribute("stroke"), len: d.length, head: d.slice(0, 50), tail: d.slice(-50) };
      });
      const rows = [...document.querySelectorAll("#model-body tr")];
      out.modelRows = rows.length;
      out.modelTableRows = rows.map(tr => { const r = tr.getBoundingClientRect(); return { top: Math.round(r.top), bottom: Math.round(r.bottom), text: tr.textContent.replace(/\\s+/g, " ").slice(0, 46) }; });
      out.docHeight = document.documentElement.scrollHeight;
      out.ovCols = getComputedStyle(document.querySelector(".ov-grid")).gridTemplateColumns;
      out.aFrom = (document.getElementById("a-from") || {}).value;
      out.aTo = (document.getElementById("a-to") || {}).value;
      return JSON.stringify(out);
    })()`;
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    console.log(r.result.value);
    const shot = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(SHOT, Buffer.from(shot.data, "base64"));
    ws.close();
  } finally {
    cleanup();
  }
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
