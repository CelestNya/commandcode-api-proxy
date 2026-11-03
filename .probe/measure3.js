/* probe 3: model panel internal geometry */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9334;
async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-profile-4", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 30; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: "http://127.0.0.1:8799/webui/#/stats" });
  await new Promise((r) => setTimeout(r, 2500));
  const r = await send("Runtime.evaluate", { expression: `(() => {
    const out = {};
    const panel = document.getElementById("model-donut").closest(".panel");
    const pr = panel.getBoundingClientRect();
    out.panel = { top: Math.round(pr.top), bottom: Math.round(pr.bottom), h: Math.round(pr.height) };
    const walk = (node, depth) => {
      const r = node.getBoundingClientRect();
      out["d" + depth + "_" + (node.id || node.className || node.tagName).toString().split(" ")[0]] = Math.round(r.top) + "→" + Math.round(r.bottom) + " h" + Math.round(r.height);
      if (depth < 3) [...node.children].forEach(c => walk(c, depth + 1));
    };
    walk(panel, 0);
    const tr = document.querySelector(".ov-grid").getBoundingClientRect();
    out.ovGrid = { top: Math.round(tr.top), bottom: Math.round(tr.bottom) };
    const trend = document.getElementById("trend-chart").closest(".panel").getBoundingClientRect();
    out.trendPanel = { top: Math.round(trend.top), bottom: Math.round(trend.bottom), h: Math.round(trend.height) };
    return JSON.stringify(out, null, 1);
  })()`, returnByValue: true });
  console.log(r.result.value);
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
