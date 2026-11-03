/* 像素级对比度实测：果冻主题的三张图。
   玻璃面板底色是半透明白叠在粉色渐层上，computed style 拿不到最终呈现色，
   所以直接从截图里采样：
     - 面板底色：图表绘图区内的空白像素（网格线之间）
     - 柱/线颜色：取该元素像素的中位色
   然后算 WCAG 对比度。 */
const { spawn } = require("child_process");
const zlib = require("zlib");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9349;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8795/webui";

function lum(rgb) {
  const c = rgb.map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); });
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}
function ratio(a, b) { const l1 = lum(a), l2 = lum(b); const hi = Math.max(l1, l2), lo = Math.min(l1, l2); return (hi + 0.05) / (lo + 0.05); }

/* 最小 PNG 解码：IHDR + IDAT(zlib inflate) + unfilter，支持 RGBA/RGB 8bit */
function decodePNG(buf) {
  let p = 8, w = 0, h = 0, bitDepth = 0, colorType = 0;
  const idat = [];
  while (p < buf.length) {
    const len = buf.readUInt32BE(p); const type = buf.toString("ascii", p + 4, p + 8);
    const data = buf.slice(p + 8, p + 8 + len);
    if (type === "IHDR") { w = data.readUInt32BE(0); h = data.readUInt32BE(4); bitDepth = data[8]; colorType = data[9]; }
    else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    p += 12 + len;
  }
  if (bitDepth !== 8) throw new Error("bitDepth " + bitDepth);
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : colorType === 0 ? 1 : 0;
  if (!channels) throw new Error("colorType " + colorType);
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const stride = w * channels;
  const out = Buffer.alloc(h * stride);
  let rp = 0;
  for (let y = 0; y < h; y++) {
    const f = raw[rp++];
    const row = raw.slice(rp, rp + stride); rp += stride;
    const cur = out.slice(y * stride, (y + 1) * stride);
    const prev = y > 0 ? out.slice((y - 1) * stride, y * stride) : Buffer.alloc(stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= channels ? cur[x - channels] : 0;
      const b = prev[x];
      const c = x >= channels ? prev[x - channels] : 0;
      let v = row[x];
      if (f === 1) v += a; else if (f === 2) v += b; else if (f === 3) v += (a + b) >> 1;
      else if (f === 4) { const pp = a + b - c; const pa = Math.abs(pp - a), pb = Math.abs(pp - b), pc = Math.abs(pp - c); v += (pa <= pb && pa <= pc) ? a : (pb <= pc ? b : c); }
      cur[x] = v & 0xff;
    }
  }
  return { w, h, channels, data: out };
}
function px(img, x, y) { const i = (y * img.w + x) * img.channels; return [img.data[i], img.data[i + 1], img.data[i + 2]]; }

async function main() {
  const chrome = spawn(CHROME, ["--headless", "--disable-gpu", "--no-sandbox", `--remote-debugging-port=${debugPort}`, "--window-size=1440,1200", "--user-data-dir=/tmp/cdp-profile-c3", "about:blank"], { stdio: "ignore" });
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
  const evalJs = async (expr) => (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;

  async function runTheme(theme) {
    await send("Page.navigate", { url: `${BASE}/#/stats` });
    await new Promise((r) => setTimeout(r, 2400));
    await evalJs(`(async () => {
      document.documentElement.setAttribute("data-theme", "${theme}");
      document.querySelector('.window-picker a[data-h="720"]').click();
      await new Promise(r => setTimeout(r, 1600));
      return 1;
    })()`);
    // 取得关键元素的屏幕坐标与 computed 颜色
    const geo = await evalJs(`(() => {
      const g = (sel) => { const e = document.querySelector(sel); if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.left, y: r.top, w: r.width, h: r.height }; };
      const cs = (sel) => { const e = document.querySelector(sel); return e ? getComputedStyle(e) : null; };
      const tok = cs("#trend-tokens .t-bar"), cost = cs("#trend-cost .t-bar");
      const line = cs("#trend-rate .trend-line"), req = cs("#trend-rate .t-bar");
      return {
        token: g("#trend-tokens"), cost: g("#trend-cost"), rate: g("#trend-rate"),
        panel: g(".trend-chart"),
        barTok: g("#trend-tokens .t-bar"), barCost: g("#trend-cost .t-bar"),
        line: g("#trend-rate .trend-line"), reqBar: g("#trend-rate .t-bar"),
        tokenBarFill: tok ? tok.fill : null, tokenBarOpacity: tok ? tok.fillOpacity : null,
        costBarFill: cost ? cost.fill : null, costBarOpacity: cost ? cost.fillOpacity : null,
        rateLineStroke: line ? line.stroke : null,
        reqBarFill: req ? req.fill : null, reqBarOpacity: req ? req.fillOpacity : null,
      };
    })()`);
    const colors = {};
    for (const k of ["tokenBarFill", "tokenBarOpacity", "costBarFill", "costBarOpacity", "rateLineStroke", "reqBarFill", "reqBarOpacity"]) {
      const v = geo[k];
      if (v == null) { colors[k] = null; continue; }
      if (k.endsWith("Opacity")) { colors[k] = v; continue; }
      const m = v.match(/rgba?\(([^)]+)\)/);
      colors[k] = m ? m[1].split(",").slice(0, 3).map(Number) : null;
    }
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    const img = decodePNG(Buffer.from(data, "base64"));
    require("fs").writeFileSync(`.probe/shots/px-${theme}.png`, Buffer.from(data, "base64"));

    const pick = (g) => {
      if (!g) return null;
      // 在元素包围盒内取与底色差异最大的像素：中心点常落在抗锯齿边缘或
      // 柱间空隙，取"差异最大"才是元素的核心色。
      const x0 = Math.max(0, Math.round(g.x)), y0 = Math.max(0, Math.round(g.y));
      const x1 = Math.min(img.w - 1, Math.round(g.x + g.w)), y1 = Math.min(img.h - 1, Math.round(g.y + g.h));
      let best = null, bestD = -1;
      for (let y = y0; y <= y1; y++) {
        for (let x = x0; x <= x1; x++) {
          const c = px(img, x, y);
          const d = Math.abs(c[0] - bgSpot[0]) + Math.abs(c[1] - bgSpot[1]) + Math.abs(c[2] - bgSpot[2]);
          if (d > bestD) { bestD = d; best = c; }
        }
      }
      return best;
    };
    // 面板底色：图表右下空白区（避开柱和线）
    const bgSpot = (() => {
      const g = geo.rate;
      if (!g) return null;
      const x = Math.round(g.x + g.w - 8), y = Math.round(g.y + 6);
      return px(img, x, y);
    })();
    console.log(`\n===== ${theme} =====`);
    console.log("面板底色(采样):", bgSpot);
    /* 柱/线的最终呈现色 = computed fill × opacity 与底色混合。
       像素采样会落在抗锯齿边缘，故这里用 CSS 声明值精确混合。 */
    const mix = (rgb, o) => rgb.map((c, i) => c * o + bgSpot[i] * (1 - o));
    const rows = [
      ["Token 柱", geo.barTok, "tokenBarFill", "tokenBarOpacity"],
      ["费用条", geo.barCost, "costBarFill", "costBarOpacity"],
      ["命中率线", geo.line, "rateLineStroke", null],
      ["请求数淡柱", geo.reqBar, "reqBarFill", "reqBarOpacity"],
    ];
    for (const [name, g, fillKey, opKey] of rows) {
      const col = colors[fillKey];
      if (!col) { console.log(name, "-> 无色值"); continue; }
      const o = opKey && colors[opKey] != null ? parseFloat(colors[opKey]) : 1;
      const eff = mix(col, o);
      console.log(`${name}: ${col.join(",")} @${o} -> rgb(${eff.map(Math.round).join(",")}) vs 底色 ${ratio(eff, bgSpot).toFixed(2)}:1`);
    }

    /* 三个系列色逐一核对（Token 图里第一根柱是 Input，Output/Cache Read
       取不到独立元素，直接读 token 声明值与面板底色比）。 */
    if (bgSpot) {
      const vars = await evalJs(`(() => {
        const s = getComputedStyle(document.documentElement);
        const g = (n) => s.getPropertyValue(n).trim();
        return { primary: g('--primary'), chartOut: g('--chart-out'),
                 chart7: g('--chart-7'), cost: g('--cost'), hit: g('--chart-hit') };
      })()`);
      const hex2rgb = (h) => {
        h = h.replace("#", "");
        if (h.length === 3) h = h.split("").map((c) => c + c).join("");
        return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16));
      };
      console.log("--- 系列色 vs 面板底色 ---");
      for (const [k, v] of Object.entries(vars || {})) {
        if (!v || v[0] !== "#" || v.length !== 7) { console.log(`  ${k}: ${v || "(空)"}`); continue; }
        const rgb = hex2rgb(v);
        const r = ratio(rgb, bgSpot);
        console.log(`  ${k.padEnd(10)} ${v} -> ${r.toFixed(2)}:1 ${r >= 4.5 ? "OK" : "!! 不达标"}`);
      }
    }
  }

  await runTheme("jelly");
  await runTheme("light");
  await runTheme("dark");
  process.exit(0);
}
main();
