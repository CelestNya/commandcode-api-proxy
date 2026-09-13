// CC Proxy Tray — personal tray manager for commandcode-api-proxy.
//
// 无主窗口：托盘图标 + 右键菜单（启动/停止/日志/开机自启/退出）。
// 代理作为 node 子进程运行，挂入 Job Object——托盘无论怎么死，子进程一并回收，
// 不会留下占用端口的孤儿。崩溃自动重启（3s）。
//
// 单实例 + 接管：互斥锁保证只有一个托盘；新实例启动时通知旧实例优雅退出
// （event 信号 → 旧实例停代理 → 释放锁 → 新实例等待端口释放后接续服务），
// 旧版本（不认识信号的）超时后按进程名兜底结束。
//
// 出站代理：默认读取 Windows 系统代理（注册表 ProxyEnable/ProxyServer），
// 注入 HTTPS_PROXY/HTTP_PROXY + NODE_USE_ENV_PROXY=1 给 node 子进程；
// NO_PROXY 恒含 localhost。环境变量 CC_PROXY=off 可关闭，CC_PROXY=<url> 可覆盖。
//
// Build (Windows 自带 .NET Framework 4.x 编译器，注意 csc 只支持 C# 5 语法):
//   csc /target:winexe /out:CCProxyTray.exe ^
//     /r:System.dll /r:System.Core.dll /r:System.Drawing.dll /r:System.Windows.Forms.dll ^
//     Tray.cs

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Linq;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;
using Microsoft.Win32;

namespace CCProxyTray
{
    static class Program
    {
        public const string MutexName = "cc-proxy-tray";
        public const string ShutdownEventName = "cc-proxy-tray-shutdown";

        // Job Object: 托盘进程死亡（含硬杀）时回收子进程
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool SetInformationJobObject(IntPtr hJob, int infoType, IntPtr lpInfo, int length);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        const int JobObjectExtendedLimitInformation = 9;
        const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;

        [StructLayout(LayoutKind.Sequential)]
        struct IO_COUNTERS
        {
            public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
            public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public ulong MinimumWorkingSetSize;
            public ulong MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public IntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public ulong ProcessMemoryLimit;
            public ulong JobMemoryLimit;
            public ulong PeakProcessMemoryUsed;
            public ulong PeakJobMemoryUsed;
        }

        static IntPtr jobHandle;

        static void CreateJob()
        {
            jobHandle = CreateJobObject(IntPtr.Zero, null);
            if (jobHandle == IntPtr.Zero) return;
            var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            var size = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
            var ptr = Marshal.AllocHGlobal(size);
            try
            {
                Marshal.StructureToPtr(info, ptr, false);
                SetInformationJobObject(jobHandle, JobObjectExtendedLimitInformation, ptr, size);
            }
            finally { Marshal.FreeHGlobal(ptr); }
        }

        [STAThread]
        static void Main()
        {
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            CreateJob();

            bool createdNew;
            var mutex = new Mutex(true, MutexName, out createdNew);
            bool tookOver = false;
            if (!createdNew)
            {
                // 通知旧实例优雅退出（新实例认识该信号）
                try
                {
                    EventWaitHandle existing;
                    if (EventWaitHandle.TryOpenExisting(ShutdownEventName, out existing))
                    {
                        existing.Set();
                        existing.Dispose();
                    }
                }
                catch { /* 旧实例可能已退出 */ }

                // 等旧实例释放锁；若它是不认识信号的老版本，按进程名兜底结束
                var sw = Stopwatch.StartNew();
                bool acquired = false;
                while (sw.ElapsedMilliseconds < 15000)
                {
                    try { if (mutex.WaitOne(400)) { acquired = true; break; } }
                    catch (AbandonedMutexException) { acquired = true; break; }
                }
                if (!acquired)
                {
                    foreach (var p in Process.GetProcessesByName("CCProxyTray"))
                    {
                        try { if (p.Id != Process.GetCurrentProcess().Id) p.Kill(); } catch { }
                    }
                    var sw2 = Stopwatch.StartNew();
                    while (sw2.ElapsedMilliseconds < 5000)
                    {
                        try { if (mutex.WaitOne(400)) { acquired = true; break; } }
                        catch (AbandonedMutexException) { acquired = true; break; }
                    }
                }
                if (!acquired)
                {
                    MessageBox.Show("旧实例未能退出，本次启动取消。", "CC Proxy Tray");
                    mutex.Dispose();
                    return;
                }
                tookOver = true;
            }

            // 拿到锁之后再创建/重置事件：旧实例退出时已消费信号，无竞态
            var shutdownEvent = new EventWaitHandle(false, EventResetMode.ManualReset, ShutdownEventName);
            shutdownEvent.Reset();

            var ctx = new TrayContext(tookOver);
            var watcher = new Thread(delegate() { shutdownEvent.WaitOne(); ctx.RequestQuit(); });
            watcher.IsBackground = true;
            watcher.Start();
            Application.Run(ctx);

            try { mutex.ReleaseMutex(); } catch { }
            mutex.Dispose();
            shutdownEvent.Dispose();
        }

        public static void AssignToJob(Process p)
        {
            if (jobHandle != IntPtr.Zero)
            {
                try { AssignProcessToJobObject(jobHandle, p.Handle); } catch { }
            }
        }
    }

    class TrayContext : ApplicationContext
    {
        const string LogFileName = "proxy.log";
        const long LogMaxBytes = 2 * 1024 * 1024;

        readonly NotifyIcon icon;
        readonly string projectRoot;
        readonly System.Windows.Forms.Timer quitPoll;
        Process proxy;
        bool quitting;          // 用户要求退出 —— 不自动重启
        bool manualStop;        // 用户要求停止 —— 不自动重启
        volatile bool quitRequested;
        readonly object gate = new object();

        public TrayContext(bool tookOver)
        {
            projectRoot = ResolveProjectRoot();

            // 先建菜单再建图标：RefreshMenu 会触碰 NotifyIcon
            var strip = BuildMenu();
            icon = new NotifyIcon
            {
                Icon = MakeIcon(Color.Gray),
                Text = "CC Proxy — stopped",
                Visible = true,
                ContextMenuStrip = strip,
            };
            icon.DoubleClick += delegate { OpenLog(); };

            // 接管信号经非 UI 线程置位，由 UI Timer 轮询执行退出（线程安全）
            quitPoll = new System.Windows.Forms.Timer();
            quitPoll.Interval = 400;
            quitPoll.Tick += delegate { if (quitRequested) Quit(); };
            quitPoll.Start();

            Directory.CreateDirectory(LogDir());
            if (tookOver)
            {
                WaitForPortFree(Port(), 8000);
                AppendLog("[tray] 接管：旧实例已退出，端口已释放");
            }
            LogEgressDecision();
            StartProxy(); // 托盘起来 = 代理起来
            RefreshMenu();
        }

        public void RequestQuit() { quitRequested = true; }

        ContextMenuStrip BuildMenu()
        {
            var strip = new ContextMenuStrip();
            menuStart = Add(strip, "启动代理", delegate { StartProxy(); });
            menuStop = Add(strip, "停止代理", delegate { StopProxy(); });
            strip.Items.Add(new ToolStripSeparator());
            menuLog = Add(strip, "打开日志", delegate { OpenLog(); });
            menuAutostart = Add(strip, "开机自启", delegate { ToggleAutostart(); });
            strip.Items.Add(new ToolStripSeparator());
            menuQuit = Add(strip, "退出（同时停止代理）", delegate { Quit(); });
            strip.Opening += delegate { RefreshCacheMenu(); };
            return strip;
        }

        ToolStripMenuItem Add(ContextMenuStrip strip, string text, EventHandler onClick)
        {
            var item = new ToolStripMenuItem(text);
            item.Click += onClick;
            strip.Items.Add(item);
            return item;
        }

        ToolStripMenuItem menuStart, menuStop, menuLog, menuAutostart, menuQuit;
        ToolStripMenuItem menuCache, menuCacheDetail;

        // 最近 24h 的缓存率：读 node 侧落盘的 logs\usage.jsonl 聚合。
        // 每次展开菜单时刷新，读取/解析失败一律显示为无数据。
        void RefreshCacheMenu()
        {
            try
            {
                var path = Path.Combine(LogDir(), "usage.jsonl");
                if (!File.Exists(path))
                {
                    menuCache.Text = "24h 缓存率：无数据";
                    menuCacheDetail.Text = " ";
                    return;
                }
                var cutoff = DateTime.UtcNow.AddHours(-24);
                double prompt = 0, cached = 0, comp = 0;
                int n = 0;
                var rx = new System.Text.RegularExpressions.Regex(
                    "\"ts\":\"([^\"]+)\".*\"promptTokens\":(\\d+),\"cachedTokens\":(\\d+),\"completionTokens\":(\\d+)");
                foreach (var line in File.ReadLines(path))
                {
                    var m = rx.Match(line);
                    if (!m.Success) continue;
                    DateTime ts;
                    if (!DateTime.TryParse(m.Groups[1].Value, null,
                        System.Globalization.DateTimeStyles.RoundtripKind, out ts)) continue;
                    if (ts < cutoff) continue;
                    prompt += double.Parse(m.Groups[2].Value);
                    cached += double.Parse(m.Groups[3].Value);
                    comp += double.Parse(m.Groups[4].Value);
                    n += 1;
                }
                if (n == 0)
                {
                    menuCache.Text = "24h 缓存率：无数据";
                    menuCacheDetail.Text = " ";
                    return;
                }
                var rate = prompt > 0 ? Math.Round(cached / prompt * 1000) / 10 : 0;
                menuCache.Text = "24h 缓存率：" + rate + "%（" + n + " 次请求）";
                menuCacheDetail.Text = "缓存 " + FmtTokens(cached) + " / " + FmtTokens(prompt)
                    + " tokens · 输出 " + FmtTokens(comp);
            }
            catch { menuCache.Text = "24h 缓存率：读取失败"; menuCacheDetail.Text = " "; }
        }

        static string FmtTokens(double v)
        {
            if (v >= 1000000) return Math.Round(v / 1000000, 1) + "M";
            if (v >= 1000) return Math.Round(v / 1000, 1) + "k";
            return Math.Round(v).ToString();
        }

        void RefreshMenu()
        {
            bool running = proxy != null && !proxy.HasExited;
            menuStart.Enabled = !running;
            menuStop.Enabled = running;
            menuAutostart.Checked = AutostartEnabled();
            icon.Icon = MakeIcon(running ? Color.FromArgb(46, 204, 113) : Color.Gray);
            icon.Text = running ? "CC Proxy — running (:" + Port() + ")" : "CC Proxy — stopped";
        }

        // ── 路径 ─────────────────────────────────────────────

        string ExeDir() { return AppDomain.CurrentDomain.BaseDirectory; }

        // exe 可能在子目录（tray\）；dist 在 <root>\dist。探测 exe 目录与其父目录。
        string ResolveProjectRoot()
        {
            var candidates = new[] { ExeDir(), Path.Combine(ExeDir(), "..") };
            foreach (var c in candidates)
            {
                try
                {
                    if (File.Exists(Path.Combine(c, "dist", "proxy.js")))
                        return Path.GetFullPath(c);
                }
                catch { }
            }
            return ExeDir();
        }

        string ProxyJs() { return Path.Combine(projectRoot, "dist", "proxy.js"); }
        string LogDir() { return Path.Combine(projectRoot, "logs"); }
        string LogPath() { return Path.Combine(LogDir(), LogFileName); }
        int Port() { return 8787; } // 与 config.ts 默认端口保持一致

        // ── node 定位：包内 node\ 优先，其次 PATH，再退到常见安装位 ──

        string FindNode()
        {
            var found = new List<string>();
            var roots = new[]
            {
                Path.Combine(ExeDir(), "node"),
                null,
                @"C:\Program Files\nodejs",
                @"C:\Program Files (x86)\nodejs",
            };
            foreach (var root in roots)
            {
                try
                {
                    var candidate = root == null ? "node.exe" : Path.Combine(root, "node.exe");
                    if (File.Exists(candidate)) found.Add(candidate);
                }
                catch { }
            }
            foreach (var dir in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(';'))
            {
                if (dir.Trim().Length == 0) continue;
                try
                {
                    var p = Path.Combine(dir.Trim(), "node.exe");
                    if (File.Exists(p) && !found.Contains(p)) found.Add(p);
                }
                catch { }
            }
            return found.FirstOrDefault();
        }

        // ── 出站代理：默认跟随 Windows 系统代理 ─────────────

        string ResolveProxyUrl()
        {
            var ovr = Environment.GetEnvironmentVariable("CC_PROXY");
            if (ovr == "off") return null;
            if (!string.IsNullOrWhiteSpace(ovr)) return NormalizeProxy(ovr);
            try
            {
                using (var key = Registry.CurrentUser.OpenSubKey(
                    @"Software\Microsoft\Windows\CurrentVersion\Internet Settings"))
                {
                    if (key == null) return null;
                    var en = key.GetValue("ProxyEnable") as int?;
                    if (en == null || en.Value != 1) return null;
                    var server = key.GetValue("ProxyServer") as string;
                    if (string.IsNullOrWhiteSpace(server)) return null;
                    if (server.IndexOf(';') >= 0)
                    {
                        // 形如 "http=...;https=...;ftp=..." —— https 优先
                        string best = null;
                        foreach (var part in server.Split(';'))
                        {
                            var kv = part.Split('=');
                            if (kv.Length == 2 && (kv[0] == "https" || kv[0] == "http"))
                            {
                                best = kv[0] + "://" + kv[1];
                                if (kv[0] == "https") break;
                            }
                        }
                        return best;
                    }
                    if (server.IndexOf('=') < 0) return "http://" + server;
                    return null;
                }
            }
            catch { return null; }
        }

        string NormalizeProxy(string url)
        {
            url = url.Trim();
            if (url.IndexOf("://") < 0) return "http://" + url;
            return url;
        }

        string BuildNoProxy()
        {
            var list = new List<string> { "localhost", "127.0.0.1" };
            try
            {
                using (var key = Registry.CurrentUser.OpenSubKey(
                    @"Software\Microsoft\Windows\CurrentVersion\Internet Settings"))
                {
                    var ovr = key != null ? key.GetValue("ProxyOverride") as string : null;
                    if (!string.IsNullOrWhiteSpace(ovr))
                    {
                        foreach (var part in ovr.Split(';'))
                            if (part.Trim().Length > 0 && part.Trim() != "<local>")
                                list.Add(part.Trim());
                    }
                }
            }
            catch { }
            return string.Join(",", list.Distinct().ToArray());
        }

        void LogEgressDecision()
        {
            var url = ResolveProxyUrl();
            AppendLog("[tray] 出站代理: " + (url ?? "直连（系统代理未开或 CC_PROXY=off）")
                      + "  [NODE_USE_ENV_PROXY=1, NO_PROXY=" + BuildNoProxy() + "]");
        }

        void ApplyEgressEnv(ProcessStartInfo psi)
        {
            psi.EnvironmentVariables["NODE_USE_ENV_PROXY"] = "1";
            var url = ResolveProxyUrl();
            if (url != null)
            {
                psi.EnvironmentVariables["HTTPS_PROXY"] = url;
                psi.EnvironmentVariables["HTTP_PROXY"] = url;
            }
            psi.EnvironmentVariables["NO_PROXY"] = BuildNoProxy();
        }

        // ── 代理生命周期 ─────────────────────────────────────

        void WaitForPortFree(int port, int timeoutMs)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < timeoutMs)
            {
                try
                {
                    var l = new TcpListener(System.Net.IPAddress.Loopback, port);
                    l.Start();
                    l.Stop();
                    return; // 端口已空闲
                }
                catch { /* 仍被占用 */ }
                Thread.Sleep(250);
            }
            AppendLog("[tray] 等待端口 " + port + " 释放超时，仍尝试启动");
        }

        void StartProxy()
        {
            lock (gate)
            {
                if (proxy != null && !proxy.HasExited) return;
                var node = FindNode();
                if (node == null || !File.Exists(ProxyJs()))
                {
                    MessageBox.Show(
                        "找不到 node.exe 或 dist\\proxy.js。\n请确认托盘 exe 位于 CCProxy 包内且已构建（pnpm build）。",
                        "CC Proxy Tray", MessageBoxButtons.OK, MessageBoxIcon.Error);
                    return;
                }
                manualStop = false;

                var psi = new ProcessStartInfo
                {
                    FileName = node,
                    Arguments = "\"" + ProxyJs() + "\"",
                    WorkingDirectory = projectRoot,
                    UseShellExecute = false,
                    CreateNoWindow = true,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                };
                ApplyEgressEnv(psi);
                var p = new Process { StartInfo = psi, EnableRaisingEvents = true };
                // node 输出是 UTF-8；不显式指定会被按系统代码页（GBK）解码成乱码
                psi.StandardOutputEncoding = System.Text.Encoding.UTF8;
                psi.StandardErrorEncoding = System.Text.Encoding.UTF8;
                p.OutputDataReceived += delegate(object s, DataReceivedEventArgs e) { AppendLog(e.Data); };
                p.ErrorDataReceived += delegate(object s, DataReceivedEventArgs e) { AppendLog(e.Data); };
                p.Exited += delegate { OnProxyExited(); };
                p.Start();
                p.BeginOutputReadLine();
                p.BeginErrorReadLine();
                Program.AssignToJob(p); // 托盘死则子进程死，无孤儿占端口
                proxy = p;
            }
            RefreshMenu();
        }

        void StopProxy()
        {
            lock (gate)
            {
                manualStop = true;
                if (proxy != null && !proxy.HasExited)
                {
                    try { proxy.Kill(); } catch { }
                }
            }
            RefreshMenu();
        }

        void OnProxyExited()
        {
            if (quitting || manualStop) { RefreshMenu(); return; }
            AppendLog("[tray] 代理意外退出 —— 3 秒后自动重启");
            System.Threading.Timer restart = null;
            restart = new System.Threading.Timer(delegate
            {
                try { restart.Dispose(); } catch { }
                StartProxy();
            });
            restart.Change(3000, Timeout.Infinite);
        }

        void Quit()
        {
            quitting = true;
            StopProxy();
            icon.Visible = false;
            Application.Exit();
        }

        // ── 日志 / 自启 ─────────────────────────────────────

        void AppendLog(string line)
        {
            if (line == null) return;
            try
            {
                lock (gate)
                {
                    var path = LogPath();
                    if (File.Exists(path) && new FileInfo(path).Length > LogMaxBytes)
                        File.WriteAllText(path, ""); // 个人用：简单截断轮转
                    File.AppendAllText(path, DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss ") + line + Environment.NewLine);
                }
            }
            catch { }
        }

        void OpenLog()
        {
            var path = LogPath();
            if (!File.Exists(path)) File.WriteAllText(path, "");
            Process.Start(new ProcessStartInfo("notepad.exe", "\"" + path + "\"") { UseShellExecute = true });
        }

        const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
        const string RunValue = "CC Proxy Tray";

        bool AutostartEnabled()
        {
            using (var key = Registry.CurrentUser.OpenSubKey(RunKey))
            {
                if (key == null) return false;
                return key.GetValue(RunValue) != null;
            }
        }

        void ToggleAutostart()
        {
            using (var key = Registry.CurrentUser.CreateSubKey(RunKey))
            {
                if (AutostartEnabled()) key.DeleteValue(RunValue, false);
                else key.SetValue(RunValue, "\"" + Application.ExecutablePath + "\"");
            }
            RefreshMenu();
        }

        // ── 托盘图标：彩色圆点，无需 .ico 文件 ───────────────

        Icon MakeIcon(Color color)
        {
            using (var bmp = new Bitmap(16, 16))
            using (var g = Graphics.FromImage(bmp))
            {
                g.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
                using (var brush = new SolidBrush(color))
                    g.FillEllipse(brush, 2, 2, 12, 12);
                using (var pen = new Pen(Color.FromArgb(60, 60, 60)))
                    g.DrawEllipse(pen, 2, 2, 12, 12);
                return Icon.FromHandle(bmp.GetHicon());
            }
        }
    }
}
