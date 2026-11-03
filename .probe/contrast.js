/* 两主题对比度实测：取关键文字元素，沿祖先链合成背景，算 WCAG 对比度。
 * 用法：node contrast.js（需实例跑在 18924） */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9377;
const BASE = "http://127.0.0.1:18924/webui/";

const EXPR = `JSON.stringify((() => {
  const parse = (c) => { const m = (c.match(/[\\d.]+/g) || []).map(Number); return { r: m[0], g: m[1], b: m[2], a: m.length > 3 ? m[3] : 1 }; };
  const over = (f, b) => ({ r: f.r * f.a + b.r * (1 - f.a), g: f.g * f.a + b.g * (1 - f.a), b: f.b * f.a + b.b * (1 - f.a), a: 1 });
  const lum = (c) => { const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); }; return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b); };
  const ratio = (a, b) => { const l1 = lum(a), l2 = lum(b); const hi = Math.max(l1, l2), lo = Math.min(l1, l2); return (hi + 0.05) / (lo + 0.05); };
  const bgOf = (el) => {
    const stack = [];
    for (let e = el; e; e = e.parentElement) {
      const c = parse(getComputedStyle(e).backgroundColor);
      if (c.a > 0) stack.push(c);
      if (c.a === 1) break;
    }
    let base = { r: 255, g: 255, b: 255, a: 1 };
    for (let i = stack.length - 1; i >= 0; i--) base = over(stack[i], base);
    return base;
  };
  const sels = [
    [".mcard .mlabel", "指标卡标签"], [".mcard .mvalue", "指标卡数字"], [".mcard .msub", "指标卡副文本"],
    [".mcard.accent .mvalue", "指标卡-紫"], [".mcard.cost .mvalue", "指标卡-钱"], [".mcard.ok .mvalue", "指标卡-青"],
    [".panel h2", "面板标题"], [".legend span", "图例"], [".win-range", "窗口范围"],
    [".topbar .chip", "顶栏 chip"], [".nav-item", "导航项"], [".nav-item.active", "导航项-选中"],
    [".window-picker a.active", "预设-选中"],
    [".model-table th", "模型表头"], [".model-table .m-name", "模型名"], [".model-table .m-cost", "模型费用"],
    ["th", "请求表头"], ["td", "表格正文"], [".badge.ok", "徽章 ok"], [".badge.error", "徽章 error"],
    [".rid-cell", "请求 ID"], [".cost-cell", "行费用"], [".tokens-cell .down", "输入 token"],
    [".tokens-cell .up", "输出 token"], [".filterbar label", "筛选标签"], [".f-actions button", "筛选按钮"],
    [".pager", "分页行"], [".side-foot", "侧栏页脚"]
  ];
  const out = [];
  for (const [sel, name] of sels) {
    const el = document.querySelector(sel);
    if (!el) { out.push([name, sel, null, 0]); continue; }
    const bg = bgOf(el);
    const fg = over(parse(getComputedStyle(el).color), bg);
    out.push([name, sel, Math.round(ratio(fg, bg) * 100) / 100, parseFloat(getComputedStyle(el).fontSize)]);
  }
  return out;
})())`;

async function run() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${port}`, "--window-size=1440,900", "--user-data-dir=/tmp/cdp-contrast", "about:blank"], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });
  let target = null;
  for (let i = 0; i < 40; i++) {
    try { const l = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); target = l.find((t) => t.type === "page"); if (target) break; } catch {}
    await new Promise((r) => setTimeout(r, 400));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => { const mid = ++id; const h = (e) => { const x = JSON.parse(e.data); if (x.id === mid) { ws.removeEventListener("message", h); x.error ? rej(new Error(x.error.message)) : res(x.result); } }; ws.addEventListener("message", h); ws.send(JSON.stringify({ id: mid, method: m, params: p })); });
  await send("Runtime.enable", {}); await send("Page.enable", {});

  const results = {};
  for (const theme of ["light", "dark"]) {
    for (const view of ["stats", "attempts"]) {
      await send("Page.navigate", { url: `${BASE}?theme=${theme}#/${view}` });
      await new Promise((r) => setTimeout(r, 2600));
      const r = await send("Runtime.evaluate", { expression: EXPR, returnByValue: true });
      results[`${theme}/${view}`] = JSON.parse(r.result.value);
    }
  }
  ws.close();

  const rows = results["light/stats"];
  console.log("元素".padEnd(16) + "字号  浅色   深色");
  console.log("-".repeat(46));
  for (let i = 0; i < rows.length; i++) {
    const light = rows[i];
    const dark = results["dark/stats"][i] || [];
    const flag = (v) => (v == null ? " —  " : (v < 3 ? String(v).padStart(5) + "!" : String(v).padStart(5) + " "));
    console.log(light[0].padEnd(16) + String(light[3] || "-").padStart(4) + "  " + flag(light[2]) + "  " + flag(dark[2]));
  }
  console.log("\n明细页：");
  for (let i = 0; i < results["light/attempts"].length; i++) {
    const l = results["light/attempts"][i];
    if (!/徽章|请求 ID|行费用|token|表头|表格正文|筛选|分页/.test(l[0])) continue;
    const d = results["dark/attempts"][i] || [];
    console.log(l[0].padEnd(16) + String(l[3] || "-").padStart(4) + "  " + String(l[2]).padStart(5) + "  " + String(d[2]).padStart(5));
  }
}
run().then(() => process.exit(0)).catch((e) => { console.error("ERR", e.message); process.exit(1); });
