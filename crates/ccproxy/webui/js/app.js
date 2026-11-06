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
       : h.indexOf("attempts") >= 0 ? "attempts"
       : h.indexOf("settings") >= 0 ? "settings" : "stats";
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

/* 窗口有两种模式：
   - preset: 选"近1小时/24h/7天/30天"。窗口相对"现在"动态滑动——每次取数都
     重算 from=(now-h, now)，曲线右端随轮询推进、旧点从左侧滑出，这才是
     new-api 式实时趋势；固定 from/to 的窗口是死的（曲线永不随时间变化）。
   - custom: 用户填了起止时间，窗口固定，轮询期间不滑动。
   state.window 只存 custom 的固定边界；preset 存小时数。 */
function currentWindow() {
  if (state.windowMode === "preset") return { from: isoAgo(state.windowHours), to: isoNow() };
  return state.window || { from: null, to: null };
}

/* state.window stores second-granular UTC ISO (no Z); the datetime-local
   inputs are local wall time. Convert both ways when syncing the picker. */
function dtLocalFromIso(iso) {
  var d = new Date(iso.indexOf("Z") >= 0 ? iso : iso + "Z");
  function p(x) { return (x < 10 ? "0" : "") + x; }
  return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate()) +
         "T" + p(d.getHours()) + ":" + p(d.getMinutes());
}
function rangeText(from, to) {
  function short(iso) {
    if (!iso) return "—";
    var d = new Date(iso.indexOf("Z") >= 0 ? iso : iso + "Z");
    function p(x) { return (x < 10 ? "0" : "") + x; }
    return p(d.getMonth() + 1) + "-" + p(d.getDate()) + " " + p(d.getHours()) + ":" + p(d.getMinutes());
  }
  return from && to ? short(from) + " ~ " + short(to) : "";
}

/* Sync the two inputs + the range label to the active window. */
function syncWindowPickerUI() {
  var w = currentWindow();
  el("w-from").value = w && w.from ? dtLocalFromIso(w.from) : "";
  el("w-to").value = w && w.to ? dtLocalFromIso(w.to) : "";
  el("window-range").textContent = rangeText(w && w.from, w && w.to);
}
/* Poll-time refresh of the range label only (preset windows slide, so the
   label should move with them); never touch the inputs mid-edit. */
function updateRangeLabel() {
  var w = currentWindow();
  var txt = rangeText(w && w.from, w && w.to);
  if (el("window-range").textContent !== txt) el("window-range").textContent = txt;
}
function selectPreset(h) {
  state.windowMode = "preset";
  state.windowHours = h;
  syncWindowPickerUI();
  fetchStats(true);
}
function applyCustom(from, to) {
  state.windowMode = "custom";
  state.window = { from: from, to: to };
  syncWindowPickerUI();
  fetchStats(true);
}
function wireWindowPicker() {
  document.querySelectorAll(".window-picker a[data-h]").forEach(function (a) {
    a.addEventListener("click", function () {
      var h = Number(a.getAttribute("data-h"));
      document.querySelectorAll(".window-picker a[data-h]").forEach(function (x) { x.classList.remove("active"); });
      a.classList.add("active");
      selectPreset(h);
    });
  });
  el("w-apply").addEventListener("click", function () {
    var f = el("w-from").value, t = el("w-to").value;
    if (!f && !t) { syncWindowPickerUI(); return; } /* empty custom range → show what's active */
    /* datetime-local is local wall time; convert to second-granular UTC ISO
       (the backend adds the right-closed boundary suffix itself). */
    var from = f ? new Date(f).toISOString().slice(0, 19) : null;
    var to = t ? new Date(t).toISOString().slice(0, 19) : null;
    document.querySelectorAll(".window-picker a[data-h]").forEach(function (x) { x.classList.remove("active"); });
    applyCustom(from, to);
  });
}

/* ── details window (independent state, R1) ── */
/* aWindow 未设置时回退到概览当前窗口（默认 24h 预设，与后端
   DEFAULT_WINDOW_HOURS 一致）——否则两个输入框从首屏就是空的占位符，
   看起来像没初始化。 */
function syncAttemptsWindowUI() {
  /* state.aWindow 初始是 {from:null,to:null}（真值对象），只有真正设过
     自定义窗口（from 非空）才用它，否则回退概览当前窗口。 */
  var w = (state.aWindow && state.aWindow.from) ? state.aWindow : currentWindow();
  el("a-from").value = w && w.from ? dtLocalFromIso(w.from) : "";
  el("a-to").value = w && w.to ? dtLocalFromIso(w.to) : "";
}
function wireAttemptsWindow() {
  el("a-apply").addEventListener("click", function () {
    var f = el("a-from").value, t = el("a-to").value;
    if (!f && !t) {
      /* 清空 + 应用 = 放弃固定窗口，回到随轮询滑动的默认窗口 */
      state.aWindow = null;
      syncAttemptsWindowUI();
      state.page = 1;
      fetchAttempts();
      fetchStats(false);
      return;
    }
    state.aWindow = {
      from: f ? new Date(f).toISOString().slice(0, 19) : null,
      to: t ? new Date(t).toISOString().slice(0, 19) : null
    };
    syncAttemptsWindowUI();
    state.page = 1;
    fetchAttempts();
    fetchStats(false);
  });
}

document.addEventListener("DOMContentLoaded", function () {
  route();
  /* 静态的那几个下拉一次到位；之后重建出来的（分页条、设置页）各自调一次。 */
  Dropdown.enhanceAll();
  wireWindowPicker();
  wireAttemptsWindow();
  /* Default: live 24h preset (matches the backend's DEFAULT_WINDOW_HOURS);
     sync the picker inputs + range label so the selector looks done from
     first paint. */
  state.windowMode = "preset";
  state.windowHours = 24;
  syncWindowPickerUI();
  syncAttemptsWindowUI(); /* 明细时段框首屏即有值（跟随概览窗口） */
  initSettings(); /* 设置页：更新检测 + 配置读写 */
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

  /* 侧栏脚注的端口必须是真正在服务的那个：写死 8787 会让任何改过端口的
     实例在界面上说谎（8894 的实例写着 8787），排查时最容易被带偏。
     location 本身就是事实，不用问后端。 */
  var sidePort = el("side-port");
  if (sidePort) {
    sidePort.textContent = "端口 " +
      (location.port || (location.protocol === "https:" ? "443" : "80"));
  }

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
