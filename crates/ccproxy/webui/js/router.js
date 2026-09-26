/* 路由
 *
 * 按 hash 切换三个视图（概览/明细/日志），并负责日志轮询随视图开关。
 */

/* ── routing (sidebar) ── */
function route() {
  var h = location.hash || "#/stats";
  var view = h.indexOf("logs") >= 0 ? "logs"
           : h.indexOf("attempts") >= 0 ? "attempts" : "stats";
  ["stats", "attempts", "logs"].forEach(function (name) {
    el("view-" + name).classList.toggle("active", name === view);
    el("nav-" + name).classList.toggle("active", name === view);
  });
  /* The log follow poll is only worth running while the screen is visible;
     route() is the one place that knows, so it owns start/stop. */
  if (view === "logs") startLogPolling(); else stopLogPolling();
}
window.addEventListener("hashchange", route);
