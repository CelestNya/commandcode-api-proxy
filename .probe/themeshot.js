/* one-off: capture the three themes of theme-switch.html at 1280 and 760 */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9361;
const URL = "file:///D:/Projects/cc-proxy/webui-concepts/theme-switch.html";
const OUT = "D:/Projects/cc-proxy/_shots/";

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1280,1000", "--user-data-dir=/tmp/cdp-theme2", "about:blank"], { stdio: "ignore" });
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
  await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  await new Promise((r) => setTimeout(r, 400));

  const themes = ["flat", "jelly", "contrast"];
  for (const t of themes) {
    await send("Runtime.evaluate", { expression: `document.documentElement.dataset.theme='${t}'` });
    await new Promise((r) => setTimeout(r, 350));
    const shot = await send("Page.captureScreenshot", { format: "png", captureBeyondViewport: true });
    fs.writeFileSync(`${OUT}theme-${t}-1280.png`, Buffer.from(shot.data, "base64"));
    console.log("shot", t, 1280);
  }
  await send("Emulation.setDeviceMetricsOverride", { width: 760, height: 1100, deviceScaleFactor: 1, mobile: false });
  await send("Runtime.evaluate", { expression: `document.documentElement.dataset.theme='jelly'` });
  await new Promise((r) => setTimeout(r, 500));
  const s2 = await send("Page.captureScreenshot", { format: "png", captureBeyondViewport: true });
  fs.writeFileSync(`${OUT}theme-jelly-760.png`, Buffer.from(s2.data, "base64"));

  // geometry / readability audit across themes
  const audit = await send("Runtime.evaluate", {
    expression: `JSON.stringify((()=>{const out={};['flat','jelly','contrast'].forEach(t=>{document.documentElement.dataset.theme=t;const cs=getComputedStyle(document.documentElement);const card=document.querySelector('.card');const ccs=getComputedStyle(card);out[t]={bg:cs.getPropertyValue('--bg').trim(),cardBg:cs.getPropertyValue('--surface').trim(),blur:ccs.backdropFilter,radius:ccs.borderRadius,shadow:ccs.boxShadow.slice(0,40),panelW:document.querySelector('.panel').getBoundingClientRect().width,overflowX:document.documentElement.scrollWidth>window.innerWidth}});document.documentElement.dataset.theme='flat';return out})())`,
    returnByValue: true
  });
  console.log(audit.result.value);
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
