/* 路由
 *
 * 按 hash 切换三个视图（概览/明细/日志），并负责日志轮询随视图开关。
 */

/* ── routing (sidebar) ── */
function route() {
  var h = location.hash || "#/stats";
  var view = h.indexOf("logs") >= 0 ? "logs"
           : h.indexOf("attempts") >= 0 ? "attempts"
           : h.indexOf("settings") >= 0 ? "settings" : "stats";
  ["stats", "attempts", "logs", "settings"].forEach(function (name) {
    el("view-" + name).classList.toggle("active", name === view);
    el("nav-" + name).classList.toggle("active", name === view);
  });
  /* The log follow poll is only worth running while the screen is visible;
     route() is the one place that knows, so it owns start/stop. */
  if (view === "logs") startLogPolling(); else stopLogPolling();
  /* 设置页不轮询：进入时取一次文档与更新状态，之后只在用户动作时刷新。 */
  if (view === "settings" && typeof onEnterSettings === "function") onEnterSettings();
}
window.addEventListener("hashchange", route);
