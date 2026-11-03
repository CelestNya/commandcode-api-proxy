/* delivery screenshots: 3 views x 3 widths */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9338;
const BASE = "http://127.0.0.1:8799/webui/";
const OUT = "D:/Projects/cc-proxy/_shots/webui-v3/";
const JOBS = [
  ["stats", 1440], ["stats", 1280], ["stats", 680],
  ["attempts", 1440], ["attempts", 1280], ["attempts", 680],
  ["logs", 1440], ["logs", 1280], ["logs", 680],
];

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-shots", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  await send("Page.navigate", { url: BASE + "#/stats" });
  await new Promise((r) => setTimeout(r, 3000));
  for (const [view, w] of JOBS) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: 900, deviceScaleFactor: 1, mobile: false });
    await new Promise((r) => setTimeout(r, 400));
    await send("Page.navigate", { url: BASE + "#/" + view });
    await new Promise((r) => setTimeout(r, 2200));
    const shot = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(`${OUT}${view}-${w}.png`, Buffer.from(shot.data, "base64"));
    console.log("shot", view, w);
  }
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
