/* one-off: screenshot the trend-split sketch at two widths */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9344;
const URL = "file:///D:/Projects/cc-proxy/webui-concepts/trend-split.html";
const OUT = "D:/Projects/cc-proxy/_shots/";

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1200,1000", "--user-data-dir=/tmp/cdp-split", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 400));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  await send("Page.navigate", { url: URL });
  await new Promise((r) => setTimeout(r, 1200));
  for (const w of [1200, 760]) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: 980, deviceScaleFactor: 1, mobile: false });
    await new Promise((r) => setTimeout(r, 500));
    const shot = await send("Page.captureScreenshot", { format: "png", captureBeyondViewport: true });
    fs.writeFileSync(`${OUT}split-${w}.png`, Buffer.from(shot.data, "base64"));
    console.log("shot", w);
  }
  // sanity: report rendered geometry
  const geo = await send("Runtime.evaluate", { expression: `JSON.stringify((()=>{const s=[...document.querySelectorAll('svg')].map(e=>({h:e.getAttribute('height'),w:e.getAttribute('viewBox'),rects:e.querySelectorAll('rect').length,paths:e.querySelectorAll('polyline').length,texts:e.querySelectorAll('text').length}));return {svgs:s,panel:document.querySelector('.panel').getBoundingClientRect().width,bodyH:document.body.scrollHeight}})())`, returnByValue: true });
  console.log(geo.result.value);
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
