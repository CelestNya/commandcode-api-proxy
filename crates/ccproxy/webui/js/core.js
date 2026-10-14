/* 核心工具
 *
 * state/PALETTE 全局量，以及 fmt* / esc 等格式化函数。其余文件都依赖它。
 */

"use strict";
var state = { stats: null, attempts: [], days: 14, page: 1, pageSize: 50, daysFetchAt: 0, filters: { model: "", stream: "", status: "" } };
/* Chart series palette, expressed as CSS variables (see tokens.css): the first
   four slots map to the semantic status colours, the rest to dedicated chart
   hues. SVG presentation attributes accept `var(--…)`, so a theme change in
   tokens.css re-colours every chart without touching JS. */
var PALETTE = ["var(--primary)", "var(--ok)", "var(--aborted)", "var(--error)",
  "var(--chart-hit)", "var(--chart-6)", "var(--chart-7)", "var(--chart-8)",
  "var(--chart-9)", "var(--chart-10)"];

function el(id) { return document.getElementById(id); }
function fmtNum(n) { return (n == null) ? "—" : Number(n).toLocaleString("en-US"); }
function fmtUsd(v) { if (v == null || isNaN(v)) return "—"; return v >= 1 ? "$" + v.toFixed(2) : "$" + v.toFixed(4); }
function fmtTokens(n) {
  if (n == null) return "—";
  if (n >= 1e9) return (n / 1e9).toFixed(2) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}
function fmtPct(v) { return (v == null) ? "—" : (v * 100).toFixed(1) + "%"; }
function fmtDur(ms) {
  if (ms == null) return "—";
  if (ms >= 3600000) return (ms / 3600000).toFixed(2) + "h";
  if (ms >= 60000) return (ms / 60000).toFixed(1) + "m";
  return (ms / 1000).toFixed(2) + "s";
}
function fmtTime(ts) {
  if (!ts) return "—";
  var d = new Date(ts);
  function p(x) { return (x < 10 ? "0" : "") + x; }
  return d.getFullYear() + "/" + p(d.getMonth() + 1) + "/" + p(d.getDate()) + " " +
         p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
}
function fmtUptime(s) {
  if (s == null) return "—";
  var d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  function p(x) { return (x < 10 ? "0" : "") + x; }
  return (d > 0 ? d + "d " : "") + p(h) + ":" + p(m);
}
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
  });
}
