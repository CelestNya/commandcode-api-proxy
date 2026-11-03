/* pixcmp2 probe: 归一化坐标的像素对照 + 整体色调统计
 * 参考图与我们的截图尺寸不同（1600x1000 vs 1440x900），
 * 采样点用相对坐标，两边自动映射，避开布局差异带来的错位。
 */
const fs = require("fs");
const zlib = require("zlib");

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
  const ch = colorType === 6 ? 4 : colorType === 2 ? 3 : 0;
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
      const b = prev[x], c = x >= ch ? prev[x - ch] : 0;
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
/* 相对坐标采样：rx/ry 是相对宽高的比例 */
function sample(img, rx, ry, rw, rh) {
  const x0 = Math.round(rx * img.w), y0 = Math.round(ry * img.h);
  const w = Math.round(rw * img.w), h = Math.round(rh * img.h);
  let r = 0, g = 0, b = 0, n = 0;
  for (let y = y0; y < Math.min(img.h, y0 + h); y++) {
    for (let x = x0; x < Math.min(img.w, x0 + w); x++) {
      const c = px(img, x, y); r += c[0]; g += c[1]; b += c[2]; n++;
    }
  }
  return n ? [Math.round(r / n), Math.round(g / n), Math.round(b / n)] : null;
}
/* 全图最深的像素簇（文字笔画） */
function darkestCluster(img) {
  const counts = new Map();
  for (let y = 0; y < img.h; y += 2) {
    for (let x = 0; x < img.w; x += 2) {
      const c = px(img, x, y);
      if (lum(c) < 0.12) {
        const k = c.map(v => Math.round(v / 8) * 8).join(",");
        const e = counts.get(k) || { n: 0, sum: [0, 0, 0] };
        e.n++; e.sum[0] += c[0]; e.sum[1] += c[1]; e.sum[2] += c[2];
        counts.set(k, e);
      }
    }
  }
  let best = null, bestN = 0;
  for (const e of counts.values()) {
    if (e.n > bestN) {
      bestN = e.n;
      best = e.sum.map(v => Math.round(v / e.n));
    }
  }
  return best;
}
/* 全图统计：平均色、平均饱和度 */
function globalStats(img) {
  let r = 0, g = 0, b = 0, n = 0, sat = 0;
  for (let y = 0; y < img.h; y += 3) {
    for (let x = 0; x < img.w; x += 3) {
      const c = px(img, x, y);
      r += c[0]; g += c[1]; b += c[2]; n++;
      const mx = Math.max(c[0], c[1], c[2]), mn = Math.min(c[0], c[1], c[2]);
      sat += mx ? (mx - mn) / mx : 0;
    }
  }
  return { avg: [Math.round(r / n), Math.round(g / n), Math.round(b / n)], sat: (sat / n).toFixed(3) };
}

const ref = decodePNG(fs.readFileSync(".probe/shots/ref-dashboard.png"));
const ours = decodePNG(fs.readFileSync(".probe/shots/jelly-now.png"));

console.log(`参考 ${ref.w}x${ref.h} | 我们 ${ours.w}x${ours.h}\n`);

console.log("=== 整体色调 ===");
const gs = globalStats(ref), go = globalStats(ours);
console.log(`参考 平均 rgb(${gs.avg}) 饱和度 ${gs.sat}`);
console.log(`我们 平均 rgb(${go.avg}) 饱和度 ${go.sat}`);

console.log("\n=== 区域像素对照（归一化坐标） ===");
const spots = [
  { name: "页面底·左上角", rx: 0.22, ry: 0.005, rw: 0.05, rh: 0.012 },
  { name: "页面底·右下角", rx: 0.72, ry: 0.60, rw: 0.05, rh: 0.012 },
  { name: "页面底·中部空白", rx: 0.45, ry: 0.62, rw: 0.04, rh: 0.010 },
  { name: "侧栏表面", rx: 0.03, ry: 0.35, rw: 0.03, rh: 0.03 },
  { name: "主区卡片表面", rx: 0.30, ry: 0.20, rw: 0.03, rh: 0.015 },
];
for (const s of spots) {
  const rc = sample(ref, s.rx, s.ry, s.rw, s.rh);
  const oc = sample(ours, s.rx, s.ry, s.rw, s.rh);
  const d = Math.round(Math.hypot(rc[0] - oc[0], rc[1] - oc[1], rc[2] - oc[2]));
  console.log(`${s.name}\n  参考 rgb(${rc})  我们 rgb(${oc})  Δ${d}`);
}

console.log("\n=== 文字色簇（笔画主色） ===");
const rt = darkestCluster(ref), ot = darkestCluster(ours);
console.log(`参考 rgb(${rt})  我们 rgb(${ot})`);
if (rt && ot) {
  // 文字与各自背景的对比
  const rb = sample(ref, 0.22, 0.005, 0.05, 0.012);
  const ob = sample(ours, 0.22, 0.005, 0.05, 0.012);
  console.log(`文字对比度  参考 ${ratio(rt, rb).toFixed(2)}:1  我们 ${ratio(ot, ob).toFixed(2)}:1`);
}
