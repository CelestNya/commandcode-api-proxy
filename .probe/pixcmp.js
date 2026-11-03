/* pixcmp probe: 像素级对比参考截图与我们的果冻主题
 * 直接从 PNG 采样，避开 computed style 与实际呈现的差异（玻璃是合成结果）。
 * 对比维度：卡片区域、页面底、玻璃边缘、阴影落点、文字颜色。
 */
const fs = require("fs");
const { spawn } = require("child_process");
const zlib = require("zlib");

const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const debugPort = 9348;

/* ── 极简 PNG 解码（只处理 8-bit RGB/RGBA，非隔行） ── */
function decodePNG(buf) {
  let pos = 8, w = 0, h = 0, bitDepth = 0, colorType = 0;
  const idat = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString("ascii", pos + 4, pos + 8);
    const data = buf.slice(pos + 8, pos + 8 + len);
    if (type === "IHDR") {
      w = data.readUInt32BE(0); h = data.readUInt32BE(4);
      bitDepth = data[8]; colorType = data[9];
      if (data[12] !== 0) throw new Error("隔行 PNG 不支持");
    } else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    pos += 12 + len;
  }
  if (bitDepth !== 8) throw new Error("仅支持 8-bit，实际 " + bitDepth);
  const ch = colorType === 6 ? 4 : colorType === 2 ? 3 : 0;
  if (!ch) throw new Error("仅支持 RGB/RGBA");
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const stride = w * ch;
  const out = Buffer.alloc(h * stride);
  let rp = 0;
  for (let y = 0; y < h; y++) {
    const ft = raw[rp++];
    const row = raw.slice(rp, rp + stride); rp += stride;
    const cur = out.slice(y * stride, (y + 1) * stride);
    const prev = y > 0 ? out.slice((y - 1) * stride, y * stride) : Buffer.alloc(stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= ch ? cur[x - ch] : 0;
      const b = prev[x];
      const c = x >= ch ? prev[x - ch] : 0;
      let v = row[x];
      if (ft === 1) v += a;
      else if (ft === 2) v += b;
      else if (ft === 3) v += (a + b) >> 1;
      else if (ft === 4) {
        const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        v += (pa <= pb && pa <= pc) ? a : (pb <= pc ? b : c);
      }
      cur[x] = v & 255;
    }
  }
  return { w, h, ch, data: out };
}
const px = (img, x, y) => {
  const i = y * img.w * img.ch + x * img.ch;
  return [img.data[i], img.data[i + 1], img.data[i + 2]];
};
const lum = (c) => {
  const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); };
  return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]);
};
const ratio = (a, b) => {
  const l1 = lum(a), l2 = lum(b);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
};
/* 区域内取与周围差异最大的像素（避开抗锯齿边缘，取元素核心色） */
function corePixel(img, x0, y0, w, h) {
  let best = null, bestD = -1;
  for (let y = Math.max(0, y0); y < Math.min(img.h, y0 + h); y++) {
    for (let x = Math.max(0, x0); x < Math.min(img.w, x0 + w); x++) {
      const c = px(img, x, y);
      const d = c[0] + c[1] + c[2];
      if (d > bestD) { bestD = d; best = c; }
    }
  }
  return best;
}
function avgPixel(img, x0, y0, w, h) {
  let r = 0, g = 0, b = 0, n = 0;
  for (let y = Math.max(0, y0); y < Math.min(img.h, y0 + h); y++) {
    for (let x = Math.max(0, x0); x < Math.min(img.w, x0 + w); x++) {
      const c = px(img, x, y); r += c[0]; g += c[1]; b += c[2]; n++;
    }
  }
  return n ? [Math.round(r / n), Math.round(g / n), Math.round(b / n)] : null;
}

async function main() {
  // 参考截图已在 .probe/shots/ref-dashboard.png
  const ref = decodePNG(fs.readFileSync(".probe/shots/ref-dashboard.png"));
  const ours = decodePNG(fs.readFileSync(".probe/shots/jelly-now.png"));
  console.log(`参考 ${ref.w}x${ref.h} | 我们 ${ours.w}x${ours.h}`);

  // 采样点（按各图自身的布局标定）
  const spots = [
    { name: "页面底·左上", ref: [240, 8, 60, 12], ours: [300, 8, 60, 12] },
    { name: "页面底·右下", ref: [1000, 620, 60, 12], ours: [1000, 640, 60, 12] },
    { name: "卡片表面·空白区", ref: [240, 60, 40, 20], ours: [230, 55, 40, 20] },
    { name: "卡片内·浅色块", ref: [700, 200, 30, 20], ours: [250, 200, 30, 20] },
    { name: "侧栏表面", ref: [30, 300, 40, 30], ours: [30, 300, 40, 30] },
  ];
  console.log("\n=== 像素对照 ===");
  for (const s of spots) {
    const rc = avgPixel(ref, ...s.ref);
    const oc = avgPixel(ours, ...s.ours);
    console.log(`${s.name}\n  参考 rgb(${rc})  我们 rgb(${oc})  Δ色距 ${Math.round(Math.hypot(rc[0]-oc[0], rc[1]-oc[1], rc[2]-oc[2]))}`);
  }

  // 文字颜色采样：取最深的像素（文字笔画）
  function darkest(img, x0, y0, w, h) {
    let best = null, bestL = 999;
    for (let y = Math.max(0, y0); y < Math.min(img.h, y0 + h); y++) {
      for (let x = Math.max(0, x0); x < Math.min(img.w, x0 + w); x++) {
        const c = px(img, x, y), l = lum(c);
        if (l < bestL) { bestL = l; best = c; }
      }
    }
    return best;
  }
  const rt = darkest(ref, 228, 88, 60, 14);   // 参考 "128,456" 数值
  const ot = darkest(ours, 228, 88, 60, 14);  // 我们 "0" 数值
  console.log(`\n=== 文字色 ===\n  参考 rgb(${rt})  我们 rgb(${ot})`);

  // 阴影落点：参考卡片下沿下方 ~14px 处
  const rs = avgPixel(ref, 300, 136, 40, 6);
  const os = avgPixel(ours, 300, 136, 40, 6);
  console.log(`\n=== 卡片下方阴影带 ===\n  参考 rgb(${rs})  我们 rgb(${os})`);

  process.exit(0);
}
main().catch((e) => { console.error("ERR", e); process.exit(1); });
