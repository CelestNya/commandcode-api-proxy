/* 装配
 *
 * DOMContentLoaded 时接上所有事件、首次取数，并按视图开关各自的轮询。
 *
 * "实时"是视图级的完整响应轮询（概览 1s、明细 2s、日志 0.7s），且只在
 * 对应视图可见、页面处于前台时运行——后台标签页一个请求都不发。
 */

/* ── wiring ── */
function currentView() {
  var h = location.hash || "#/stats";
  return h.indexOf("logs") >= 0 ? "logs"
       : h.indexOf("attempts") >= 0 ? "attempts" : "stats";
}

/* 概览/明细的轮询走同一定时器：一个 tick 按当前视图取对应的数。 */
var statsTimer = null;
function startDataPolling() {
  if (statsTimer) return;
  statsTimer = setInterval(function () {
    if (document.hidden) return;
    var view = currentView();
    if (view === "stats") fetchStats(false);
    else if (view === "attempts") { fetchAttempts(); fetchStats(false); } /* mini cards follow the window too */
  }, 1000);
}

/* ── overview window picker (independent from the details one, R1) ── */
function isoNow() { return new Date().toISOString().slice(0, 19); }
function isoAgo(hours) { return new Date(Date.now() - hours * 3600 * 1000).toISOString().slice(0, 19); }
function setOverviewWindow(from, to) {
  state.window = { from: from, to: to };
  fetchStats(true);
}
function wireWindowPicker() {
  document.querySelectorAll(".window-picker a[data-h]").forEach(function (a) {
    a.addEventListener("click", function () {
      var h = Number(a.getAttribute("data-h"));
      document.querySelectorAll(".window-picker a[data-h]").forEach(function (x) { x.classList.remove("active"); });
      a.classList.add("active");
      setOverviewWindow(isoAgo(h), isoNow());
    });
  });
  el("w-apply").addEventListener("click", function () {
    var f = el("w-from").value, t = el("w-to").value;
    if (!f && !t) return;
    /* datetime-local is local wall time; convert to second-granular UTC ISO
       (the backend adds the right-closed boundary suffix itself). */
    var from = f ? new Date(f).toISOString().slice(0, 19) : null;
    var to = t ? new Date(t).toISOString().slice(0, 19) : null;
    document.querySelectorAll(".window-picker a[data-h]").forEach(function (x) { x.classList.remove("active"); });
    setOverviewWindow(from, to);
  });
}

/* ── details window (independent state, R1) ── */
function wireAttemptsWindow() {
  el("a-apply").addEventListener("click", function () {
    var f = el("a-from").value, t = el("a-to").value;
    if (!f && !t) return;
    state.aWindow = {
      from: f ? new Date(f).toISOString().slice(0, 19) : null,
      to: t ? new Date(t).toISOString().slice(0, 19) : null
    };
    state.page = 1;
    fetchAttempts();
    fetchStats(false);
  });
}

document.addEventListener("DOMContentLoaded", function () {
  route();
  wireWindowPicker();
  wireAttemptsWindow();
  ["f-model", "f-stream", "f-status"].forEach(function (id) {
    el(id).addEventListener("change", function () {
      state.filters[id.replace("f-", "")] = el(id).value;
      state.page = 1;
      renderAttempts(state.attempts);
    });
  });
  el("btn-reset").addEventListener("click", function () {
    state.filters = { model: "", stream: "", status: "" };
    el("f-model").value = ""; el("f-stream").value = ""; el("f-status").value = "";
    state.page = 1;
    renderAttempts(state.attempts);
  });
  el("btn-refresh").addEventListener("click", function () { fetchAttempts(); fetchStats(); });

  el("drawer-close").addEventListener("click", closeDrawer);
  el("req-drawer").addEventListener("click", function (e) {
    if (e.target === el("req-drawer") || e.target.id === "drawer-close") closeDrawer();
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && !el("req-drawer").hidden) closeDrawer();
  });

  /* Log screen: switching file restarts the follow loop; a manual scroll away
     from the bottom suspends auto-scroll, scrolling back re-arms it. */
  el("log-which").addEventListener("change", function () {
    logState.which = el("log-which").value;
    startLogPolling();
  });
  document.querySelectorAll(".lvl-filter input[data-lvl]").forEach(function (cb) {
    cb.addEventListener("change", function () {
      logState.levels[cb.getAttribute("data-lvl")] = cb.checked;
      /* Re-paint from the file so the filter applies to already-rendered
         lines too; the follow loop resumes from the fresh anchor. */
      startLogPolling();
    });
  });
  el("log-refresh").addEventListener("click", function () { startLogPolling(); });
  el("log-autoscroll").addEventListener("click", function () {
    logState.autoScroll = !logState.autoScroll;
    el("log-autoscroll").classList.toggle("active", logState.autoScroll);
    if (logState.autoScroll) scrollLogToBottom();
  });
  el("log-pane").addEventListener("scroll", function () {
    var pane = el("log-pane");
    var atBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 24;
    /* Scrolling up suspends auto-scroll; scrolling back to the bottom re-arms
       it. While auto-scrolling we move scrollTop ourselves, and that event
       arrives already at the bottom, so it never flips the toggle off. */
    if (logState.autoScroll === atBottom) return;
    logState.autoScroll = atBottom;
    el("log-autoscroll").classList.toggle("active", atBottom);
  });

  /* Astro-style static first: the page itself carries no data (which is what
     makes its ETag stable), so hydrate from the JSON APIs in parallel — the
     round-trip is loopback-scale. */
  fetchStats(true);
  fetchAttempts();
  fetchSysinfo();
  if (currentView() === "logs") startLogPolling();
  setInterval(fetchSysinfo, 5000);
  startDataPolling();

  document.addEventListener("visibilitychange", function () {
    if (document.hidden) { stopLogPolling(); }
    else if (currentView() === "logs") { startLogPolling(); }
  });
});
