/* jellyfinal probe: 果冻主题最终验收
 * 三视图（1440 / 1100 / 900）× 三主题，各截图；并对关键文字做对比度实测。
 */
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9350;
const BASE = "http://127.0.0.1:8795/webui";

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${debugPort}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-jf", "about:blank",
  ], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });

  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => {
    const mid = ++id;
    ws.onmessage = (e) => {
      const x = JSON.parse(e.data);
      if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result);
    };
    ws.send(JSON.stringify({ id: mid, method: m, params: p }));
  });
  await send("Runtime.enable", {});
  await send("Page.enable", {});
  const evalJs = async (expr) =>
    (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;
  async function shot(name) {
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    require("fs").writeFileSync(`.probe/shots/${name}.png`, Buffer.from(data, "base64"));
    console.log(`shot: ${name}`);
  }
  async function setViewport(w, h) {
    await send("Emulation.setDeviceMetricsOverride", {
      width: w, height: h, deviceScaleFactor: 1, mobile: false,
    });
  }

  // ── 三主题 × 概览页 ──
  for (const theme of ["jelly", "light", "dark"]) {
    await setViewport(1440, 900);
    await send("Page.navigate", { url: `${BASE}/?theme=${theme}#/stats` });
    await new Promise((r) => setTimeout(r, 3200));
    await evalJs(`(async () => {
      document.documentElement.setAttribute("data-theme", "${theme}");
      await new Promise(r => setTimeout(r, 500));
      return 1;
    })()`);
    await new Promise((r) => setTimeout(r, 600));
    await shot(`jf-${theme}-1440`);
  }

  // ── 果冻：明细页 + 设置页 + 窄屏 ──
  await send("Page.navigate", { url: `${BASE}/?theme=jelly#/attempts` });
  await new Promise((r) => setTimeout(r, 3000));
  await evalJs(`document.documentElement.setAttribute("data-theme","jelly")`);
  await new Promise((r) => setTimeout(r, 500));
  await shot("jf-jelly-attempts");

  await send("Page.navigate", { url: `${BASE}/?theme=jelly#/settings` });
  await new Promise((r) => setTimeout(r, 3000));
  await evalJs(`document.documentElement.setAttribute("data-theme","jelly")`);
  await new Promise((r) => setTimeout(r, 500));
  await shot("jf-jelly-settings");

  await setViewport(900, 900);
  await send("Page.navigate", { url: `${BASE}/?theme=jelly#/stats` });
  await new Promise((r) => setTimeout(r, 3000));
  await evalJs(`document.documentElement.setAttribute("data-theme","jelly")`);
  await new Promise((r) => setTimeout(r, 600));
  await shot("jf-jelly-900");

  // ── 关键文字对比度实测（果冻主题） ──
  await setViewport(1440, 900);
  await send("Page.navigate", { url: `${BASE}/?theme=jelly#/stats` });
  await new Promise((r) => setTimeout(r, 3200));
  const contrast = await evalJs(`(() => {
    const lum = (c) => {
      const f = (v) => { v /= 255; return v <= 0.03928 ? v/12.92 : Math.pow((v+0.055)/1.055, 2.4); };
      return 0.2126*f(c[0]) + 0.7152*f(c[1]) + 0.0722*f(c[2]);
    };
    // 不含反斜杠的解析：模板字符串里 \\( 会被吃掉成 (，正则就废了。
    // 这里按 "(" 与 ")" 手工截取，逗号切分。
    const parse = (s) => {
      if (!s) return null;
      const i = s.indexOf("(");
      const j = s.indexOf(")");
      if (i < 0 || j < i) return null;
      const p = s.slice(i + 1, j).split(",").map(v => parseFloat(v));
      if (p.length < 3 || p.some(v => !isFinite(v))) return null;
      return { rgb: [p[0], p[1], p[2]], a: p.length > 3 ? p[3] : 1 };
    };
    const over = (fg, bg) => fg.rgb.map((c,i) => c*fg.a + bg[i]*(1-fg.a));
    const ratio = (a, b) => {
      const l1 = lum(a), l2 = lum(b);
      return (Math.max(l1,l2)+0.05)/(Math.min(l1,l2)+0.05);
    };
    // 逐级向上找不透明祖先，合成最终底色（渐变背景按其首个色标近似）
    const effBg = (el) => {
      let stack = [];
      let n = el;
      while (n && n !== document.documentElement) {
        const cs = getComputedStyle(n);
        const c = parse(cs.backgroundColor);
        if (c && c.a > 0) { stack.push(c); if (c.a >= 1) break; }
        else if (cs.backgroundImage && cs.backgroundImage !== "none") {
          const g = parse(cs.backgroundImage);
          if (g) { stack.push(g); break; }
        }
        n = n.parentElement;
      }
      el.__stack = stack.map((s) => ({ rgb: s.rgb, a: s.a }));
      let base = [255, 235, 237];
      for (let i = stack.length - 1; i >= 0; i--) base = over(stack[i], base);
      return base;
    };
    const targets = [
      ["指标卡数值", ".mcard .mc-v, .mcard .num, .mcard b, .mcard strong"],
      ["指标卡标签", ".mcard .lbl, .mcard .mc-l, .mcard .muted, .mcard span"],
      ["面板标题", ".panel h2"],
      ["表格表头", ".model-table th"],
      ["表格数值", ".model-table td"],
      ["导航激活项", ".nav-item.active"],
      ["顶栏 chip", ".chip"],
      ["时间窗按钮", ".window-picker a:not(.active)"],
      ["时间窗激活", ".window-picker a.active"],
      ["模型名", ".model-table .m-name"],
      ["费用值", ".model-table .m-cost"],
      ["筛选标签", ".filterbar label"],
      ["趋势标题", ".trend-sec-title"],
    ];
    const out = [];
    for (const [name, sel] of targets) {
      const el = document.querySelector(sel);
      if (!el) { out.push({ name, miss: true }); continue; }
      const cs = getComputedStyle(el);
      const fg = parse(cs.color);
      const bg = effBg(el);
      if (!fg) { out.push({ name, miss: true, why: "fg parse fail: " + cs.color }); continue; }
      if (!bg || bg.some((v) => !isFinite(v))) {
        out.push({ name, miss: true, why: "bg bad: " + JSON.stringify(bg) + " stack:" + JSON.stringify(el.__stack) });
        continue;
      }
      const r = ratio(over(fg, bg), bg);
      out.push({ name, color: cs.color, size: cs.fontSize, ratio: +r.toFixed(2), pass: r >= 4.5 });
    }
    return out;
  })()`);
  console.log("\n=== 果冻主题关键文字对比度 ===");
  for (const c of contrast) {
    if (c.miss) { console.log(`  ${c.name}: 未找到`); continue; }
    console.log(`  ${c.name.padEnd(12)} ${c.color} ${c.size} -> ${c.ratio}:1 ${c.pass ? "OK" : "!! 不达标"}`);
  }
  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
