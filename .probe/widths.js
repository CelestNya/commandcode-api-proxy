/* width sweep: verify fixed-count card grids + shell + overflow across breakpoints */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1243/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9336;
const BASE = "http://127.0.0.1:8799/webui/";
const WIDTHS = [375, 680, 901, 1000, 1100, 1240, 1280, 1366, 1440, 1920];

const STATS_EXPR = `(() => {
  const q = (s) => document.querySelector(s);
  const rect = (s) => { const e = q(s); if (!e) return null; const r = e.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; };
  const out = {};
  out.shell = rect(".shell");
  out.shellDir = getComputedStyle(q(".shell")).flexDirection;
  out.side = rect(".sidebar");
  out.top = rect(".topbar");
  const mc = q(".metric-cards");
  const cards = [...mc.children];
  out.mCols = getComputedStyle(mc).gridTemplateColumns.split(" ").length;
  out.mTops = cards.map(c => c.offsetTop);
  out.mCard = cards.length ? [Math.round(cards[0].getBoundingClientRect().width), Math.round(cards[0].getBoundingClientRect().height)] : null;
  let maxLines = 0;
  cards.forEach(c => { const s = c.querySelector(".msub"); if (s) { const lh = parseFloat(getComputedStyle(s).lineHeight) || 18; maxLines = Math.max(maxLines, Math.round(s.getBoundingClientRect().height / lh)); } });
  out.mSubLines = maxLines;
  out.ovCols = getComputedStyle(q(".ov-grid")).gridTemplateColumns;
  const mw = q(".model-table-wrap");
  out.mWrap = mw ? [mw.scrollWidth, mw.clientWidth] : null;
  const mt = q(".model-table");
  out.mTableW = mt ? Math.round(mt.getBoundingClientRect().width) : null;
  const mn = q(".model-table .m-name");
  out.mNameClip = mn ? mn.scrollWidth > mn.clientWidth + 1 : null;
  out.mNumClip = [...document.querySelectorAll("#model-body td:nth-child(n+2)")].some(td => td.scrollWidth > td.clientWidth + 1);
  const c = q(".content");
  out.cOverflow = c ? c.scrollWidth > c.clientWidth + 1 : null;
  out.docEqWin = document.documentElement.scrollHeight === window.innerHeight;
  return JSON.stringify(out);
})()`;

const ATT_EXPR = `(() => {
  const q = (s) => document.querySelector(s);
  const mc = q(".mini-cards");
  const cards = [...mc.children];
  const out = {};
  out.iCols = getComputedStyle(mc).gridTemplateColumns.split(" ").length;
  out.iTops = cards.map(c => c.offsetTop);
  let maxLines = 0;
  cards.forEach(c => { const s = c.querySelector(".mc-sub"); if (s) { const lh = parseFloat(getComputedStyle(s).lineHeight) || 18; maxLines = Math.max(maxLines, Math.round(s.getBoundingClientRect().height / lh)); } });
  out.iSubLines = maxLines;
  const fb = q(".filterbar");
  out.fbH = fb ? Math.round(fb.getBoundingClientRect().height) : null;
  const groups = [...document.querySelectorAll(".filterbar .f-group")];
  out.fRows = new Set(groups.map(g => Math.round(g.getBoundingClientRect().top))).size;
  const lat = document.querySelector(".lat-cell");
  out.latClip = lat ? lat.scrollWidth > lat.clientWidth + 1 : null;
  const cc = q(".content");
  out.cOverflow = cc ? cc.scrollWidth > cc.clientWidth + 1 : null;
  out.docEqWin = document.documentElement.scrollHeight === window.innerHeight;
  return JSON.stringify(out);
})()`;

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-sweep", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = list.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!target) throw new Error("chrome did not start");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; ws.onmessage = (e) => { const x = JSON.parse(e.data); if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result); }; ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});
  const ev = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result.value;

  await send("Page.navigate", { url: BASE + "#/stats" });
  await new Promise((r) => setTimeout(r, 3000));

  for (const w of WIDTHS) {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: 900, deviceScaleFactor: 1, mobile: false });
    await new Promise((r) => setTimeout(r, 350));
    await send("Page.navigate", { url: BASE + "#/stats" });
    await new Promise((r) => setTimeout(r, 1300));
    const s = await ev(STATS_EXPR);
    await send("Page.navigate", { url: BASE + "#/attempts" });
    await new Promise((r) => setTimeout(r, 1300));
    const a = await ev(ATT_EXPR);
    console.log(`W${w} STATS ${s}`);
    console.log(`W${w} ATT   ${a}`);
  }
  ws.close();
}
main().catch((e) => { console.error("ERR", e.message); process.exit(1); });
