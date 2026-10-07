// AgentWikiTray: the per-user system tray app for Agent Wiki.
//
// The service runs in session 0 and can never show UI, so this app runs in the
// user's session, started at sign-in by a per-user logon task and the HKCU Run
// key (both without admin; see Autostart). It:
//  - polls the service's read-only GET /status on 127.0.0.1 (the only way it
//    talks to the service) and shows the state as an icon: healthy, degraded
//    (backlog, failed notes, errors) or down;
//  - hosts the curator (`node curator.mjs --parent-stdin`) as the user, because
//    the curator uses the user's own Codex sign-in, which the service account
//    must never have. It restarts the curator if it exits and when curator.mjs
//    changes on disk (so `npm run install-local` upgrades need no re-login);
//  - opens the Agent Wiki window on a left click (the web app the service serves
//    at /ui/, in an Edge app window: search, pages, activity, inbox, status);
//  - offers a menu on a right click: open the window, status, open wiki/index/
//    logs, recent activity, pause/resume the curator, curator sign-in and failed
//    notes, start at sign-in, copy the MCP URL, restart the service, quit;
//  - watches its own start-at-sign-in entries and logs (logs\tray.log) when
//    one disappears.
//
// Single instance per session (named mutex). `--quit` asks the running
// instance to exit; `--selftest` fetches /status once, prints the state and the
// menu as text, and exits (for tests); `--do <action>` runs one menu action
// without the UI; `--from <task|run>` only labels who started it in the log.
// Settings come from AgentWikiTray.ini next to the exe (written by the
// installer) or `--config <file>`.
//
// C# 5, built by the .NET Framework csc.exe that ships with Windows.

using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Net;
using System.Reflection;
using System.ServiceProcess;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Web.Script.Serialization;
using System.Windows.Forms;
using Microsoft.Win32;

// The name Windows shows for the exe (Settings > Other system tray icons, Task Manager > Startup apps).
[assembly: AssemblyTitle("Agent Wiki")]
[assembly: AssemblyProduct("Agent Wiki")]
[assembly: AssemblyDescription("Agent Wiki tray: status of the shared AI memory, and its curator")]

public static class Program
{
    // Session-local names, so each Windows session has its own tray. `instance=` in the ini changes them (tests).
    static string instance = "AgentWikiTray";
    public static string MutexName { get { return "Local\\" + instance; } }
    public static string QuitEventName { get { return "Local\\" + instance + "-Quit"; } }

    [STAThread]
    public static int Main(string[] args)
    {
        string ini = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "AgentWikiTray.ini");
        for (int i = 0; i < args.Length - 1; i++)
        {
            if (args[i] == "--config") ini = args[i + 1];
        }
        bool selftest = Array.IndexOf(args, "--selftest") >= 0;
        bool quit = Array.IndexOf(args, "--quit") >= 0;
        int fromAt = Array.IndexOf(args, "--from");
        string from = fromAt >= 0 && fromAt + 1 < args.Length ? args[fromAt + 1] : "manual";

        Config cfg;
        try
        {
            cfg = Config.Load(ini);
        }
        catch (Exception e)
        {
            if (quit) return SignalQuit(); // the default instance
            string msg = "Agent Wiki tray: cannot read " + ini + ": " + e.Message;
            if (selftest) { Console.Error.WriteLine(msg); return 2; }
            MessageBox.Show(msg + "\n\nRe-run `npm run install-local` in the agent-wiki repo.", "Agent Wiki", MessageBoxButtons.OK, MessageBoxIcon.Error);
            return 2;
        }
        if (!string.IsNullOrEmpty(cfg.Instance)) instance = cfg.Instance;
        if (quit) return SignalQuit();
        if (selftest) return SelfTest(cfg);
        int doAt = Array.IndexOf(args, "--do");
        if (doAt >= 0 && doAt + 1 < args.Length) return DoAction(cfg, args[doAt + 1]);

        bool created;
        using (Mutex mutex = new Mutex(true, MutexName, out created))
        {
            if (!created) return 3; // already running in this session (both sign-in entries start it)
            TrayLog.Write(cfg, "started pid " + Process.GetCurrentProcess().Id + " (from " + from + "): " + Autostart.ExePath);
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.Run(new TrayContext(cfg));
            TrayLog.Write(cfg, "quit");
            mutex.ReleaseMutex();
        }
        return 0;
    }

    /** Asks the running instance to quit and waits (up to 20 s) until it has. 0 = not running any more. */
    static int SignalQuit()
    {
        EventWaitHandle quit;
        try { quit = EventWaitHandle.OpenExisting(QuitEventName); }
        catch (WaitHandleCannotBeOpenedException) { return 0; }
        using (quit) quit.Set();
        DateTime deadline = DateTime.UtcNow.AddSeconds(20);
        while (DateTime.UtcNow < deadline)
        {
            try { using (Mutex.OpenExisting(MutexName)) { } }
            catch (WaitHandleCannotBeOpenedException) { return 0; }
            Thread.Sleep(200);
        }
        return 1;
    }

    /** `--do <action>`: runs one menu action without the UI (scripts, tests). Prints "ok: <message>" or "error: <message>". */
    static int DoAction(Config cfg, string action)
    {
        using (StreamWriter o = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false)))
        {
            o.NewLine = "\n";
            try
            {
                string msg = TrayActions.Run(cfg, action, null);
                o.WriteLine("ok: " + (msg ?? action));
                return 0;
            }
            catch (Exception e)
            {
                o.WriteLine("error: " + e.Message);
                return 1;
            }
        }
    }

    static int SelfTest(Config cfg)
    {
        // A winexe has no console: write UTF-8 straight to the inherited stdout handle.
        using (StreamWriter o = new StreamWriter(Console.OpenStandardOutput(), new UTF8Encoding(false)))
        {
            o.NewLine = "\n";
            Status s = Status.Fetch(cfg);
            o.WriteLine("state=" + s.State);
            o.WriteLine("tooltip=" + s.Tooltip(cfg));
            int icons = 0;
            foreach (string state in new[] { "healthy", "degraded", "down" })
            {
                using (Icon ic = TrayContext.LoadIcon(cfg, state, 16)) { if (ic != null && ic.Width == 16) icons++; }
            }
            o.WriteLine("icons=" + icons);
            Autostart.State a = Autostart.Check(cfg);
            o.WriteLine("autostart=" + a);
            o.WriteLine("ui=" + cfg.UiUrl + " browser=" + (UiWindow.Browser(cfg) ?? "(default browser)"));
            o.WriteLine("uiargs=" + UiWindow.Arguments(cfg, Screen.PrimaryScreen.WorkingArea));
            foreach (string line in TrayMenu.Describe(cfg, s, a)) o.WriteLine("item=" + line);
        }
        return 0;
    }
}

// ---------------------------------------------------------------- settings

public class Config
{
    public string Node, Runtime, WikiDir, UiProfile, LogDir, IconDir, Codex, CodexHome, Instance, Service = "AgentWiki";
    // Start at sign-in (tests point these at throwaway names).
    public string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run", RunValue = "AgentWikiTray", Task = "AgentWikiTray", TaskXml;
    public string Browser; // the Agent Wiki window's browser; default: Microsoft Edge
    public int Port = 47821, AutostartCheckSeconds = 120, UiWaitSeconds = 20;
    public bool HostCurator = true, ShowIcon = true;

    public string StatusUrl { get { return "http://127.0.0.1:" + Port + "/status"; } }
    public string McpUrl { get { return "http://127.0.0.1:" + Port + "/mcp"; } }
    public string UiUrl { get { return "http://127.0.0.1:" + Port + "/ui/"; } }

    public static Config Load(string path)
    {
        Dictionary<string, string> d = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (string raw in File.ReadAllLines(path, Encoding.UTF8))
        {
            string line = raw.Trim();
            if (line.Length == 0 || line.StartsWith("#") || line.StartsWith(";")) continue;
            int eq = line.IndexOf('=');
            if (eq > 0) d[line.Substring(0, eq).Trim()] = line.Substring(eq + 1).Trim();
        }
        foreach (string k in new[] { "node", "runtime", "wikiDir", "logDir", "icons", "port" })
        {
            if (!d.ContainsKey(k)) throw new Exception("missing key '" + k + "'");
        }
        Config c = new Config();
        c.Node = d["node"];
        c.Runtime = d["runtime"];
        c.WikiDir = d["wikiDir"];
        // The window's own Edge profile; "home" is the pre-1.3 layout (~/.agent-wiki/ui-profile).
        if (d.ContainsKey("uiProfile")) c.UiProfile = d["uiProfile"];
        else if (d.ContainsKey("home")) c.UiProfile = Path.Combine(d["home"], "ui-profile");
        else throw new Exception("missing key 'uiProfile'");
        c.LogDir = d["logDir"];
        c.IconDir = d["icons"];
        c.Port = int.Parse(d["port"]);
        if (d.ContainsKey("codex")) c.Codex = d["codex"];
        if (d.ContainsKey("codexHome")) c.CodexHome = d["codexHome"];
        if (d.ContainsKey("service")) c.Service = d["service"];
        if (d.ContainsKey("instance")) c.Instance = d["instance"];
        if (d.ContainsKey("curator")) c.HostCurator = d["curator"] != "0" && d["curator"].ToLowerInvariant() != "off";
        if (d.ContainsKey("icon")) c.ShowIcon = d["icon"] != "0"; // tests run without a visible icon
        if (d.ContainsKey("runKey")) c.RunKey = d["runKey"];
        if (d.ContainsKey("runValue")) c.RunValue = d["runValue"];
        if (d.ContainsKey("task")) c.Task = d["task"];
        c.TaskXml = d.ContainsKey("taskXml") ? d["taskXml"] : Path.Combine(Path.GetDirectoryName(Path.GetFullPath(path)), "AgentWikiTray.task.xml");
        if (d.ContainsKey("autostartCheckSeconds")) c.AutostartCheckSeconds = Math.Max(1, int.Parse(d["autostartCheckSeconds"]));
        if (d.ContainsKey("browser")) c.Browser = d["browser"];
        if (d.ContainsKey("uiWaitSeconds")) c.UiWaitSeconds = Math.Max(0, int.Parse(d["uiWaitSeconds"]));
        return c;
    }
}

// ---------------------------------------------------------------- /status

public class Status
{
    public bool Reachable;
    public string Error;
    public Dictionary<string, object> Data;

    public string State
    {
        get
        {
            if (!Reachable) return "down";
            return Str("health") == "ok" ? "healthy" : "degraded";
        }
    }

    public static Status Fetch(Config cfg)
    {
        Status s = new Status();
        try
        {
            HttpWebRequest req = (HttpWebRequest)WebRequest.Create(cfg.StatusUrl);
            req.Proxy = null;
            req.Timeout = 4000;
            req.ReadWriteTimeout = 4000;
            req.KeepAlive = false;
            using (HttpWebResponse res = (HttpWebResponse)req.GetResponse())
            using (StreamReader r = new StreamReader(res.GetResponseStream(), Encoding.UTF8))
            {
                s.Data = new JavaScriptSerializer().DeserializeObject(r.ReadToEnd()) as Dictionary<string, object>;
                s.Reachable = s.Data != null;
            }
        }
        catch (Exception e)
        {
            s.Error = e.Message;
        }
        return s;
    }

    /** Walks nested JSON objects: Get("queue", "pending"). */
    public object Get(params string[] keys)
    {
        object cur = Data;
        foreach (string k in keys)
        {
            Dictionary<string, object> d = cur as Dictionary<string, object>;
            if (d == null || !d.TryGetValue(k, out cur)) return null;
        }
        return cur;
    }

    public string Str(params string[] keys) { object v = Get(keys); return v == null ? null : Convert.ToString(v); }
    public int Int(params string[] keys) { object v = Get(keys); try { return v == null ? 0 : Convert.ToInt32(v); } catch { return 0; } }
    public bool Bool(params string[] keys) { object v = Get(keys); return v is bool && (bool)v; }

    public List<string> Reasons()
    {
        List<string> list = new List<string>();
        object[] arr = Get("reasons") as object[];
        if (arr == null) { ArrayList al = Get("reasons") as ArrayList; if (al != null) arr = al.ToArray(); }
        if (arr != null) foreach (object o in arr) list.Add(Convert.ToString(o));
        return list;
    }

    public List<Dictionary<string, object>> Recent()
    {
        List<Dictionary<string, object>> list = new List<Dictionary<string, object>>();
        object v = Get("recent");
        IEnumerable items = v as object[];
        if (items == null) items = v as ArrayList;
        if (items != null) foreach (object o in items) { Dictionary<string, object> d = o as Dictionary<string, object>; if (d != null) list.Add(d); }
        return list;
    }

    public static string Uptime(int sec)
    {
        if (sec < 90) return sec + " s";
        if (sec < 5400) return (sec / 60) + " min";
        if (sec < 172800) return (sec / 3600) + " h " + (sec % 3600 / 60) + " min";
        return (sec / 86400) + " days";
    }

    public string Tooltip(Config cfg)
    {
        string t;
        if (!Reachable) t = "Agent Wiki: service down";
        else
        {
            int q = Int("queue", "pending");
            int dead = Int("queue", "dead");
            t = "Agent Wiki " + Str("version") + ": " + (State == "healthy" ? "OK" : "needs attention");
            t += " · " + q + " queued";
            if (dead > 0) t += " · " + dead + " failed";
        }
        return t.Length > 63 ? t.Substring(0, 60) + "..." : t; // NotifyIcon.Text limit
    }
}

// ---------------------------------------------------------------- menu text

public static class TrayMenu
{
    /** The menu as lines of text ("Parent > Child"), for --selftest. Mirrors TrayContext.BuildMenu. */
    public static List<string> Describe(Config cfg, Status s, Autostart.State a)
    {
        List<string> lines = new List<string>();
        foreach (string[] item in Items(cfg, s, a)) lines.Add(item[0].Length > 0 ? item[0] + " > " + item[1] : item[1]);
        return lines;
    }

    /** [submenu ("" = top level), text, action id ("" = disabled info line)]. `a` is null until the first check. */
    public static List<string[]> Items(Config cfg, Status s, Autostart.State a)
    {
        List<string[]> m = new List<string[]>();
        m.Add(new[] { "", "Open Agent Wiki", "open-ui" });
        m.Add(new[] { "", "-", "" });
        if (!s.Reachable)
        {
            m.Add(new[] { "", "Agent Wiki: service not responding", "" });
            m.Add(new[] { "", "Apps fall back to their own server until it is back", "" });
        }
        else
        {
            m.Add(new[] { "", "Agent Wiki " + s.Str("version") + " · " + (s.State == "healthy" ? "OK" : "needs attention"), "" });
            m.Add(new[] { "", "Up " + Status.Uptime(s.Int("uptimeSec")) + " · " + s.Int("queue", "pending") + " note(s) queued" + (s.Int("queue", "dead") > 0 ? " · " + s.Int("queue", "dead") + " failed" : ""), "" });
            string lw = s.Str("lastWrite", "at");
            if (lw != null) m.Add(new[] { "", "Last write " + Clip(lw.Replace("T", " "), 16) + " · " + Clip(s.Str("lastWrite", "text"), 60), "" });
            foreach (string r in s.Reasons()) m.Add(new[] { "", "! " + Clip(r, 80), "" });
        }
        if (a != null && !a.Starts) m.Add(new[] { "", "! Will not start at sign-in: Start at sign-in > Repair", "" });
        else if (a != null && !a.Both) m.Add(new[] { "", "! Start at sign-in: " + (a.Task == "on" ? "Run key entry " + a.Run : "logon task " + a.Task), "" });
        m.Add(new[] { "", "-", "" });
        m.Add(new[] { "", "Open wiki folder", "open-wiki" });
        m.Add(new[] { "", "Open index.md", "open-index" });
        m.Add(new[] { "", "Open logs", "open-logs" });
        List<Dictionary<string, object>> recent = s.Recent();
        if (recent.Count == 0) m.Add(new[] { "Recent activity", "(nothing yet)", "" });
        foreach (Dictionary<string, object> h in recent)
        {
            string date = Convert.ToString(h["date"]);
            m.Add(new[] { "Recent activity", date.Substring(5) + " " + h["time"] + " · " + Clip(Convert.ToString(h["text"]), 70), "open-log:" + date });
        }
        m.Add(new[] { "", "-", "" });
        bool paused = s.Bool("curator", "paused") || File.Exists(PausedFlag(cfg));
        m.Add(new[] { "", paused ? "Resume curator" : "Pause curator", paused ? "resume" : "pause" });
        string cstate = s.Str("curator", "state");
        bool running = s.Bool("curator", "running");
        string line = !s.Reachable ? "state unknown" : !running ? (cfg.HostCurator ? "not running" : "not hosted by this tray") : cstate + (s.Str("curator", "model") != null ? " (" + s.Str("curator", "model") + ", " + s.Str("curator", "reasoningEffort") + ")" : "");
        m.Add(new[] { "Curator", "Curator: " + line, "" });
        if (s.Str("curator", "lastError") != null) m.Add(new[] { "Curator", "Last error: " + Clip(s.Str("curator", "lastError"), 80), "" });
        m.Add(new[] { "Curator", "Sign in to ChatGPT for the curator...", "sign-in" });
        int dead = s.Int("queue", "dead");
        m.Add(new[] { "Curator", "Retry failed notes (" + dead + ")", dead > 0 ? "retry-dead" : "" });
        m.Add(new[] { "Curator", "File failed notes as sent, without the model (" + dead + ")", dead > 0 ? "file-raw" : "" });
        m.Add(new[] { "Curator", "Restart the curator", cfg.HostCurator ? "restart-curator" : "" });
        m.Add(new[] { "Curator", "Open the curator log", "open-curator-log" });
        string sub = "Start at sign-in";
        m.Add(new[] { sub, "Logon task (Task Scheduler): " + (a == null ? "checking..." : a.Task), "" });
        m.Add(new[] { sub, "Run key entry (HKCU): " + (a == null ? "checking..." : a.Run), "" });
        m.Add(new[] { sub, "Repair: register both again", "repair-autostart" });
        m.Add(new[] { "", "Copy MCP URL (" + cfg.McpUrl + ")", "copy-url" });
        m.Add(new[] { "", "Restart service", "restart-service" });
        m.Add(new[] { "", "-", "" });
        m.Add(new[] { "", "Quit", "quit" });
        return m;
    }

    public static string PausedFlag(Config cfg) { return Path.Combine(Path.Combine(cfg.WikiDir, ".curator"), "paused"); }

    public static string Clip(string s, int n)
    {
        if (s == null) return "";
        s = s.Replace("\r", " ").Replace("\n", " ").Replace("&", "&&");
        return s.Length > n ? s.Substring(0, n - 3) + "..." : s;
    }
}

// ---------------------------------------------------------------- the tray

public class TrayContext : ApplicationContext
{
    readonly Config cfg;
    readonly NotifyIcon notify;
    readonly Dictionary<string, Icon> icons = new Dictionary<string, Icon>();
    readonly System.Windows.Forms.Timer timer;
    readonly SynchronizationContext ui;
    readonly CuratorHost curator;
    readonly EventWaitHandle quitEvent;
    readonly DateTime started = DateTime.UtcNow;
    readonly System.Windows.Forms.Timer autostartTimer;
    Status last = new Status();
    string lastState;
    int polling, checkingAutostart;
    Autostart.State autostart;
    DateTime autostartCheckedAt;

    public TrayContext(Config cfg)
    {
        this.cfg = cfg;
        foreach (string st in new[] { "healthy", "degraded", "down" }) icons[st] = LoadIcon(cfg, st, SystemInformation.SmallIconSize.Width) ?? SystemIcons.Application;
        notify = new NotifyIcon();
        notify.Icon = icons["down"];
        notify.Text = "Agent Wiki: starting";
        notify.ContextMenuStrip = new ContextMenuStrip(); // creating a control installs the WinForms sync context
        ui = SynchronizationContext.Current ?? new WindowsFormsSynchronizationContext();
        notify.ContextMenuStrip.Opening += delegate { BuildMenu(); };
        // Left click (or a double click) opens the Agent Wiki window; right click shows the menu.
        notify.MouseUp += delegate(object s, MouseEventArgs e)
        {
            if (e.Button == MouseButtons.Left) Do("open-ui");
        };
        BuildMenu();
        notify.Visible = cfg.ShowIcon;

        curator = new CuratorHost(cfg);
        if (cfg.HostCurator) curator.Start();

        quitEvent = new EventWaitHandle(false, EventResetMode.AutoReset, Program.QuitEventName);
        Thread waiter = new Thread(delegate()
        {
            try { quitEvent.WaitOne(); } catch { return; }
            ui.Post(delegate { Quit(); }, null);
        });
        waiter.IsBackground = true;
        waiter.Start();

        timer = new System.Windows.Forms.Timer();
        timer.Interval = 5000;
        timer.Tick += delegate { Poll(); };
        timer.Start();
        Poll();

        autostartTimer = new System.Windows.Forms.Timer();
        autostartTimer.Interval = cfg.AutostartCheckSeconds * 1000;
        autostartTimer.Tick += delegate { CheckAutostart(); };
        autostartTimer.Start();
        CheckAutostart();
    }

    /**
     * Logs the start-at-sign-in entries once, then every change with the time it was noticed, so a
     * vanished entry can be matched to whatever removed it. Only reports: putting an entry back is
     * your call (Repair, or install-local).
     */
    void CheckAutostart()
    {
        if (Interlocked.Exchange(ref checkingAutostart, 1) == 1) return;
        ThreadPool.QueueUserWorkItem(delegate
        {
            Autostart.State a;
            try { a = Autostart.Check(cfg); }
            catch (Exception e) { TrayLog.Write(cfg, "start-at-sign-in check failed: " + e.Message); Interlocked.Exchange(ref checkingAutostart, 0); return; }
            ui.Post(delegate
            {
                Interlocked.Exchange(ref checkingAutostart, 0);
                Autostart.State before = autostart;
                DateTime prevCheck = autostartCheckedAt;
                autostart = a;
                autostartCheckedAt = DateTime.Now;
                if (before == null) { TrayLog.Write(cfg, "start at sign-in: " + a); return; }
                if (before.ToString() == a.ToString()) return;
                TrayLog.Write(cfg, "start at sign-in changed: " + before + " -> " + a + " (between " + prevCheck.ToString("yyyy-MM-ddTHH:mm:ss") + " and now)");
                if (before.Starts && !a.Starts) Balloon("Agent Wiki will not start at sign-in", "Its logon task and Run key entry are gone. Right-click > Start at sign-in > Repair.", ToolTipIcon.Warning);
            }, null);
        });
    }

    public static Icon LoadIcon(Config cfg, string state, int size)
    {
        string file = Path.Combine(cfg.IconDir, "agent-wiki-" + state + ".ico");
        try { return File.Exists(file) ? new Icon(file, size, size) : null; }
        catch { return null; }
    }

    void Poll()
    {
        if (Interlocked.Exchange(ref polling, 1) == 1) return;
        ThreadPool.QueueUserWorkItem(delegate
        {
            Status s = Status.Fetch(cfg);
            ui.Post(delegate { Interlocked.Exchange(ref polling, 0); Apply(s); }, null);
        });
    }

    void Apply(Status s)
    {
        last = s;
        string state = s.State;
        notify.Icon = icons[state];
        notify.Text = s.Tooltip(cfg);
        if (lastState != null && state != lastState)
        {
            bool settled = (DateTime.UtcNow - started).TotalSeconds > 60;
            if (state == "down" && settled) Balloon("Agent Wiki service is not responding", "Apps fall back to their own server meanwhile. Right-click the icon to restart the service.", ToolTipIcon.Warning);
            else if (state == "degraded")
            {
                List<string> r = s.Reasons();
                Balloon("Agent Wiki needs attention", r.Count > 0 ? r[0] : "See the tray menu.", ToolTipIcon.Warning);
            }
            else if (state == "healthy" && lastState != null && settled) Balloon("Agent Wiki is back to normal", "", ToolTipIcon.Info);
        }
        lastState = state;
    }

    void Balloon(string title, string text, ToolTipIcon icon)
    {
        notify.BalloonTipTitle = title;
        notify.BalloonTipText = text.Length > 0 ? text : " ";
        notify.BalloonTipIcon = icon;
        notify.ShowBalloonTip(5000);
    }

    void BuildMenu()
    {
        ContextMenuStrip menu = notify.ContextMenuStrip;
        menu.Items.Clear();
        Dictionary<string, ToolStripMenuItem> subs = new Dictionary<string, ToolStripMenuItem>();
        bool first = true;
        foreach (string[] it in TrayMenu.Items(cfg, last, autostart))
        {
            ToolStripItemCollection target = menu.Items;
            if (it[0].Length > 0)
            {
                ToolStripMenuItem sub;
                if (!subs.TryGetValue(it[0], out sub))
                {
                    sub = new ToolStripMenuItem(it[0]);
                    subs[it[0]] = sub;
                    menu.Items.Add(sub);
                }
                target = sub.DropDownItems;
            }
            if (it[1] == "-") { target.Add(new ToolStripSeparator()); continue; }
            ToolStripMenuItem item = new ToolStripMenuItem(it[1]);
            string action = it[2];
            if (action.Length == 0) item.Enabled = false; // status lines are information only
            if (first && it[0].Length == 0) { item.Font = new Font(item.Font, FontStyle.Bold); first = false; }
            if (action.Length > 0) item.Click += delegate { Do(action); };
            target.Add(item);
        }
    }

    static readonly string[] SlowActions = { "restart-service", "retry-dead", "file-raw", "repair-autostart", "open-ui" };

    void Do(string action)
    {
        if (action == "quit") { Quit(); return; }
        if (action == "restart-service") Balloon("Restarting the Agent Wiki service", "Apps reconnect on their own.", ToolTipIcon.Info);
        if (Array.IndexOf(SlowActions, action) < 0)
        {
            try
            {
                string msg = TrayActions.Run(cfg, action, curator);
                if (msg != null) Balloon("Agent Wiki", msg, ToolTipIcon.Info);
            }
            catch (Exception e) { Balloon("Agent Wiki", e.Message, ToolTipIcon.Error); }
            Poll();
            return;
        }
        Thread t = new Thread(delegate()
        {
            string msg;
            bool failed = false;
            try { msg = TrayActions.Run(cfg, action, curator); }
            catch (Exception e) { msg = e.Message; failed = true; }
            ui.Post(delegate
            {
                if (msg != null) Balloon(failed ? "Agent Wiki: that did not work" : "Agent Wiki", msg, failed ? ToolTipIcon.Error : ToolTipIcon.Info);
                Poll();
                if (action == "repair-autostart") CheckAutostart();
            }, null);
        });
        t.IsBackground = true;
        t.Start();
    }

    void Quit()
    {
        timer.Stop();
        autostartTimer.Stop();
        notify.Visible = false;
        curator.Stop();
        notify.Dispose();
        ExitThread();
    }
}

// ---------------------------------------------------------------- menu actions

/**
 * What each menu item does. Shared by the tray (clicks) and `--do <action>`
 * (scripts and tests). Returns a message for the user, or null; throws on
 * failure. Slow actions (restart-service, retry-dead, file-raw) block.
 */
public static class TrayActions
{
    public static string Run(Config cfg, string action, CuratorHost curator)
    {
        if (action == "open-wiki") { Open(cfg.WikiDir); return null; }
        if (action == "open-index") { OpenFile(Path.Combine(cfg.WikiDir, "index.md")); return null; }
        if (action == "open-logs") { Open(cfg.LogDir); return null; }
        if (action == "open-curator-log") { OpenFile(Path.Combine(cfg.LogDir, "curator.log")); return null; }
        if (action.StartsWith("open-log:"))
        {
            string date = action.Substring(9);
            OpenFile(Path.Combine(Path.Combine(Path.Combine(cfg.WikiDir, "log"), date.Substring(0, 4)), date + ".md"));
            return null;
        }
        if (action == "pause")
        {
            string flag = TrayMenu.PausedFlag(cfg);
            Directory.CreateDirectory(Path.GetDirectoryName(flag));
            File.WriteAllText(flag, "paused from the tray at " + DateTime.Now.ToString("yyyy-MM-ddTHH:mm:sszzz") + "\n");
            return "Curator paused. Notes keep being saved and wait in the inbox.";
        }
        if (action == "resume")
        {
            string flag = TrayMenu.PausedFlag(cfg);
            if (File.Exists(flag)) File.Delete(flag);
            return "Curator resumed.";
        }
        if (action == "copy-url") { Clipboard.SetText(cfg.McpUrl); return "Copied " + cfg.McpUrl; }
        if (action == "sign-in") { SignIn(cfg, curator); return null; }
        if (action == "retry-dead") return RunCurator(cfg, "--retry-dead", "Failed notes queued for another try.");
        if (action == "file-raw") return RunCurator(cfg, "--file-raw-dead", "Failed notes filed into the log as they were sent.");
        if (action == "restart-curator")
        {
            if (curator == null) throw new Exception("This tray does not host the curator.");
            curator.Restart();
            return "Curator restarted.";
        }
        if (action == "restart-service") return RestartService(cfg);
        if (action == "repair-autostart") return Autostart.Repair(cfg);
        if (action == "open-ui") return UiWindow.Open(cfg);
        throw new Exception("Unknown action: " + action);
    }

    static void Open(string folder)
    {
        Directory.CreateDirectory(folder);
        Process.Start("explorer.exe", "\"" + folder + "\"");
    }

    static void OpenFile(string file)
    {
        if (!File.Exists(file)) throw new Exception("Not found: " + file);
        // With no app registered for the extension (.md often has none), the shell would show the
        // "How do you want to open this file?" picker; Notepad is the better default.
        bool associated = false;
        using (Microsoft.Win32.RegistryKey k = Microsoft.Win32.Registry.ClassesRoot.OpenSubKey(Path.GetExtension(file)))
        {
            associated = k != null && (k.GetValue("") != null || k.GetSubKeyNames().Length > 0);
        }
        try
        {
            if (associated) { Process.Start(new ProcessStartInfo(file) { UseShellExecute = true }); return; }
        }
        catch { }
        Process.Start("notepad.exe", "\"" + file + "\"");
    }

    static void SignIn(Config cfg, CuratorHost curator)
    {
        if (string.IsNullOrEmpty(cfg.Codex) || string.IsNullOrEmpty(cfg.CodexHome)) throw new Exception("The Codex CLI was not found when Agent Wiki was installed.");
        Directory.CreateDirectory(cfg.CodexHome);
        ProcessStartInfo psi = new ProcessStartInfo("cmd.exe", "/k title Agent Wiki curator: sign in to ChatGPT && echo This signs the Agent Wiki curator in to ChatGPT (its own Codex home: %CODEX_HOME%). && echo. && \"" + cfg.Codex + "\" login && echo. && echo Done. You can close this window.");
        psi.UseShellExecute = false;
        psi.EnvironmentVariables["CODEX_HOME"] = cfg.CodexHome;
        psi.EnvironmentVariables.Remove("CODEX_API_KEY");
        psi.EnvironmentVariables.Remove("OPENAI_API_KEY");
        Process p = Process.Start(psi);
        if (curator == null) return;
        Thread t = new Thread(delegate()
        {
            p.WaitForExit();
            curator.Restart(); // pick up the new sign-in at once
        });
        t.IsBackground = true;
        t.Start();
    }

    static string RunCurator(Config cfg, string arg, string done)
    {
        ProcessStartInfo psi = new ProcessStartInfo(cfg.Node, "\"" + Path.Combine(cfg.Runtime, "curator.mjs") + "\" " + arg);
        psi.UseShellExecute = false;
        psi.CreateNoWindow = true;
        psi.RedirectStandardOutput = true;
        psi.RedirectStandardError = true;
        using (Process p = Process.Start(psi))
        {
            string err = p.StandardError.ReadToEnd();
            p.WaitForExit(120000);
            if (p.ExitCode != 0) throw new Exception("Failed: " + TrayMenu.Clip(err, 200));
            return done;
        }
    }

    static string RestartService(Config cfg)
    {
        try
        {
            using (ServiceController sc = new ServiceController(cfg.Service))
            {
                if (sc.Status != ServiceControllerStatus.Stopped)
                {
                    sc.Stop();
                    sc.WaitForStatus(ServiceControllerStatus.Stopped, TimeSpan.FromSeconds(30));
                }
                sc.Start();
                sc.WaitForStatus(ServiceControllerStatus.Running, TimeSpan.FromSeconds(30));
            }
            return "The service restarted.";
        }
        catch (InvalidOperationException e)
        {
            bool denied = e.InnerException is System.ComponentModel.Win32Exception && ((System.ComponentModel.Win32Exception)e.InnerException).NativeErrorCode == 5;
            throw new Exception(denied
                ? "Your account may not restart the service yet. Run `node scripts\\install-service.mjs` once in an elevated terminal (it grants only start/stop of this service)."
                : e.Message + (e.InnerException != null ? " " + e.InnerException.Message : ""));
        }
        catch (System.ServiceProcess.TimeoutException) { throw new Exception("The service did not restart within 30 s; see logs\\service.log."); }
    }
}

// ---------------------------------------------------------------- start at sign-in

/**
 * Two independent per-user entries start the tray at sign-in, neither needing admin: a Task
 * Scheduler logon task (`<task>`, registered from the installer's AgentWikiTray.task.xml) and the
 * HKCU Run value. Either one is enough; the later start exits (single instance). Two, because on
 * 2026-10-02 the Run value alone vanished overnight on a managed PC, removed by something outside
 * this app. The tray reports and logs a missing entry but never re-adds one by itself (that is
 * what malware does, and security tools treat it so); Repair and install-local do.
 */
public static class Autostart
{
    public const string StartupApprovedKey = @"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    /** Each entry: "on", "missing", "disabled" (turned off by you) or "starts <other exe>". */
    public class State
    {
        public string Task, Run;
        public bool Starts { get { return Task == "on" || Run == "on"; } }
        public bool Both { get { return Task == "on" && Run == "on"; } }
        public override string ToString() { return "task:" + Task + " run:" + Run; }
    }

    public static string ExePath { get { return Path.GetFullPath(Assembly.GetEntryAssembly().Location); } }

    public static State Check(Config cfg)
    {
        State s = new State();
        s.Run = CheckRun(cfg);
        s.Task = CheckTask(cfg);
        return s;
    }

    /** The exe a command line starts: its first token, quoted or not. */
    public static string CommandExe(string command)
    {
        command = (command ?? "").Trim();
        if (command.StartsWith("\""))
        {
            int end = command.IndexOf('"', 1);
            return end > 0 ? command.Substring(1, end - 1) : command.Substring(1);
        }
        int sp = command.IndexOf(' ');
        return sp > 0 ? command.Substring(0, sp) : command;
    }

    static string Points(string command)
    {
        string exe = CommandExe(command);
        try { exe = Path.GetFullPath(Environment.ExpandEnvironmentVariables(exe)); } catch { }
        return string.Equals(exe, ExePath, StringComparison.OrdinalIgnoreCase) ? "on" : "starts " + exe;
    }

    static string CheckRun(Config cfg)
    {
        using (RegistryKey k = Registry.CurrentUser.OpenSubKey(cfg.RunKey))
        {
            string v = k == null ? null : k.GetValue(cfg.RunValue) as string;
            if (v == null) return "missing";
            // Task Manager > Startup apps keeps its on/off switch apart from the Run key: an odd first byte is off.
            if (cfg.RunKey.Equals(new Config().RunKey, StringComparison.OrdinalIgnoreCase))
            {
                using (RegistryKey ap = Registry.CurrentUser.OpenSubKey(StartupApprovedKey))
                {
                    byte[] b = ap == null ? null : ap.GetValue(cfg.RunValue) as byte[];
                    if (b != null && b.Length > 0 && (b[0] & 1) == 1) return "disabled";
                }
            }
            return Points(v);
        }
    }

    static string CheckTask(Config cfg)
    {
        string output;
        int code = Schtasks(out output, "/query", "/tn", cfg.Task, "/xml", "ONE");
        if (code != 0) return "missing";
        Match settings = Regex.Match(output, @"<Settings>[\s\S]*?</Settings>");
        if (settings.Success && Regex.IsMatch(settings.Value, @"<Enabled>\s*false\s*</Enabled>")) return "disabled";
        Match cmd = Regex.Match(output, @"<Command>([^<]*)</Command>");
        if (!cmd.Success) return "starts nothing";
        return Points("\"" + WebUtility.HtmlDecode(cmd.Groups[1].Value).Trim('"') + "\"");
    }

    /** Registers both entries again for this exe. Leaves a Run entry you turned off in Startup apps alone. */
    public static string Repair(Config cfg)
    {
        if (!File.Exists(cfg.TaskXml)) throw new Exception("No task definition at " + cfg.TaskXml + "; run `npm run install-local` once.");
        string output;
        if (Schtasks(out output, "/create", "/tn", cfg.Task, "/xml", cfg.TaskXml, "/f") != 0) throw new Exception("Task Scheduler refused the logon task: " + TrayMenu.Clip(output, 200));
        using (RegistryKey k = Registry.CurrentUser.CreateSubKey(cfg.RunKey))
        {
            k.SetValue(cfg.RunValue, "\"" + ExePath + "\" --from run", RegistryValueKind.String);
        }
        State s = Check(cfg);
        TrayLog.Write(cfg, "start at sign-in repaired: " + s);
        if (s.Run == "disabled") return "Logon task registered. The Run key entry is turned off in Task Manager > Startup apps; turn it on there if you want both.";
        return "Agent Wiki starts at sign-in again (" + s + ").";
    }

    static int Schtasks(out string output, params string[] args)
    {
        StringBuilder line = new StringBuilder();
        foreach (string a in args) line.Append(line.Length > 0 ? " " : "").Append("\"" + a.Replace("\"", "\\\"") + "\"");
        ProcessStartInfo psi = new ProcessStartInfo(Path.Combine(Environment.SystemDirectory, "schtasks.exe"), line.ToString());
        psi.UseShellExecute = false;
        psi.CreateNoWindow = true;
        psi.RedirectStandardOutput = true;
        psi.RedirectStandardError = true;
        using (Process p = Process.Start(psi))
        {
            System.Threading.Tasks.Task<string> err = p.StandardError.ReadToEndAsync();
            output = p.StandardOutput.ReadToEnd();
            if (!p.WaitForExit(30000)) { try { p.Kill(); } catch { } output = "schtasks.exe timed out"; return -1; }
            output += err.Result;
            return p.ExitCode;
        }
    }
}

/**
 * The Agent Wiki window: the web app the service serves at /ui/, in a Microsoft Edge app window
 * (no tabs or address bar; Edge is part of Windows, so nothing to install). An open window is
 * brought back to the front. A new one opens in its own Edge profile (ini uiProfile):
 * Edge honors --window-size/--window-position only when it starts a new browser process, which a
 * profile of our own makes sure of, and it reopens an app window where it was last closed. So the
 * window appears where it lands, above the tray, and is never moved after it shows (moving it
 * afterwards was a visible jump from Edge's default spot).
 */
public static class UiWindow
{
    public const string Title = "Agent Wiki"; // document.title of the web app
    const int Width = 460, Height = 760, Margin = 12;
    static DateTime launchedAt = DateTime.MinValue;
    static readonly object gate = new object();

    delegate bool EnumProc(IntPtr hwnd, IntPtr lParam);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int cmd);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
    [System.Runtime.InteropServices.DllImport("user32.dll")] static extern bool AllowSetForegroundWindow(int pid);

    /** Microsoft Edge from App Paths (machine, then user), else its usual folder; null if absent. */
    public static string Browser(Config cfg)
    {
        if (!string.IsNullOrEmpty(cfg.Browser)) return File.Exists(cfg.Browser) ? cfg.Browser : null;
        foreach (RegistryKey root in new[] { Registry.LocalMachine, Registry.CurrentUser })
        {
            using (RegistryKey k = root.OpenSubKey(@"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe"))
            {
                string p = k == null ? null : k.GetValue("") as string;
                if (!string.IsNullOrEmpty(p) && File.Exists(p.Trim('"'))) return p.Trim('"');
            }
        }
        string guess = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFilesX86), @"Microsoft\Edge\Application\msedge.exe");
        return File.Exists(guess) ? guess : null;
    }

    /** The open Agent Wiki window: a visible Chromium top-level window titled exactly "Agent Wiki". */
    public static IntPtr Find()
    {
        IntPtr found = IntPtr.Zero;
        StringBuilder text = new StringBuilder(256), cls = new StringBuilder(64);
        EnumWindows(delegate(IntPtr h, IntPtr l)
        {
            if (!IsWindowVisible(h)) return true;
            text.Length = 0;
            cls.Length = 0;
            GetWindowText(h, text, text.Capacity);
            GetClassName(h, cls, cls.Capacity);
            if (text.ToString() == Title && cls.ToString() == "Chrome_WidgetWin_1") { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static string Profile(Config cfg) { return cfg.UiProfile; }

    /** Edge's command line: our profile, the app URL, and where the window first appears (above the tray). */
    public static string Arguments(Config cfg, Rectangle workArea)
    {
        int w = Math.Min(Width, workArea.Width - 2 * Margin), h = Math.Min(Height, workArea.Height - 2 * Margin);
        return "--user-data-dir=\"" + Profile(cfg) + "\" --app=" + cfg.UiUrl +
            " --window-size=" + w + "," + h + " --window-position=" + (workArea.Right - w - Margin) + "," + (workArea.Bottom - h - Margin) +
            " --no-first-run --no-default-browser-check";
    }

    public static string Open(Config cfg)
    {
        lock (gate)
        {
            IntPtr h = Find();
            if (h != IntPtr.Zero) { Focus(h); return null; }
            if ((DateTime.UtcNow - launchedAt).TotalSeconds < 5) return null; // a double click: the first launch is still opening
            string browser = Browser(cfg);
            if (browser == null)
            {
                Process.Start(new ProcessStartInfo(cfg.UiUrl) { UseShellExecute = true });
                return "Opened Agent Wiki in your default browser (Microsoft Edge was not found).";
            }
            AllowSetForegroundWindow(-1); // let the browser come to the front
            Rectangle wa = Screen.FromPoint(Cursor.Position).WorkingArea;
            Process.Start(new ProcessStartInfo(browser, Arguments(cfg, wa)) { UseShellExecute = false, CreateNoWindow = true });
            launchedAt = DateTime.UtcNow;
        }
        DateTime deadline = DateTime.UtcNow.AddSeconds(cfg.UiWaitSeconds);
        while (DateTime.UtcNow < deadline)
        {
            IntPtr h = Find();
            if (h != IntPtr.Zero) { Focus(h); return null; }
            Thread.Sleep(50);
        }
        return null;
    }

    static void Focus(IntPtr h)
    {
        if (IsIconic(h)) ShowWindow(h, 9); // SW_RESTORE
        SetForegroundWindow(h);
    }
}

/** logs\tray.log: when the tray started and by what, and its start-at-sign-in checks. */
public static class TrayLog
{
    const long MaxBytes = 1024 * 1024;
    static readonly object gate = new object();

    public static void Write(Config cfg, string msg)
    {
        lock (gate)
        {
            try
            {
                Directory.CreateDirectory(cfg.LogDir);
                string file = Path.Combine(cfg.LogDir, "tray.log");
                FileInfo fi = new FileInfo(file);
                if (fi.Exists && fi.Length > MaxBytes)
                {
                    if (File.Exists(file + ".1")) File.Delete(file + ".1");
                    File.Move(file, file + ".1");
                }
                using (StreamWriter w = new StreamWriter(new FileStream(file, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete), new UTF8Encoding(false)))
                {
                    w.NewLine = "\n";
                    w.WriteLine(DateTime.Now.ToString("yyyy-MM-ddTHH:mm:ss.fffzzz") + " [tray] " + msg);
                }
            }
            catch { }
        }
    }
}

// ---------------------------------------------------------------- the curator, hosted as the user

public class CuratorHost
{
    const long MaxLogBytes = 5 * 1024 * 1024;
    readonly Config cfg;
    readonly ManualResetEvent stopEvent = new ManualResetEvent(false);
    readonly object logLock = new object();
    volatile bool stopping;
    volatile bool restartRequested;
    volatile Process current;
    Thread thread;
    StreamWriter log;

    public CuratorHost(Config cfg) { this.cfg = cfg; }

    public void Start()
    {
        thread = new Thread(Loop);
        thread.IsBackground = true;
        thread.Start();
    }

    public void Restart()
    {
        restartRequested = true;
        stopEvent.Set();
    }

    public void Stop()
    {
        stopping = true;
        stopEvent.Set();
        if (thread != null) thread.Join(20000);
    }

    string Script { get { return Path.Combine(cfg.Runtime, "curator.mjs"); } }

    void Log(string msg)
    {
        if (msg == null) return;
        lock (logLock)
        {
            try
            {
                if (log == null)
                {
                    Directory.CreateDirectory(cfg.LogDir);
                    string file = Path.Combine(cfg.LogDir, "curator.log");
                    FileInfo fi = new FileInfo(file);
                    if (fi.Exists && fi.Length > MaxLogBytes)
                    {
                        if (File.Exists(file + ".1")) File.Delete(file + ".1");
                        File.Move(file, file + ".1");
                    }
                    log = new StreamWriter(new FileStream(file, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete), new UTF8Encoding(false));
                    log.NewLine = "\n";
                    log.AutoFlush = true;
                }
                log.WriteLine(DateTime.Now.ToString("yyyy-MM-ddTHH:mm:ss.fffzzz") + " " + msg);
            }
            catch { }
        }
    }

    static DateTime Stamp(string f) { try { return File.GetLastWriteTimeUtc(f); } catch { return DateTime.MinValue; } }

    void Loop()
    {
        int backoff = 1000;
        while (!stopping)
        {
            stopEvent.Reset();
            restartRequested = false;
            DateTime stamp = Stamp(Script);
            Process p = new Process();
            p.StartInfo.FileName = cfg.Node;
            p.StartInfo.Arguments = "\"" + Script + "\" --parent-stdin";
            p.StartInfo.WorkingDirectory = cfg.Runtime;
            p.StartInfo.UseShellExecute = false;
            p.StartInfo.CreateNoWindow = true;
            p.StartInfo.RedirectStandardInput = true;
            p.StartInfo.RedirectStandardOutput = true;
            p.StartInfo.RedirectStandardError = true;
            p.OutputDataReceived += delegate(object s, DataReceivedEventArgs e) { Log(e.Data); };
            p.ErrorDataReceived += delegate(object s, DataReceivedEventArgs e) { Log(e.Data); };
            DateTime started = DateTime.UtcNow;
            try { p.Start(); }
            catch (Exception e)
            {
                Log("[tray] cannot start the curator (" + cfg.Node + "): " + e.Message);
                if (stopEvent.WaitOne(60000) && stopping) break;
                continue;
            }
            current = p;
            p.BeginOutputReadLine();
            p.BeginErrorReadLine();
            Log("[tray] started curator pid " + p.Id);
            bool changed = false;
            while (!p.HasExited && !stopping && !restartRequested)
            {
                if (stopEvent.WaitOne(1000)) break;
                DateTime now = Stamp(Script);
                if (now != stamp && now != DateTime.MinValue && (DateTime.UtcNow - now).TotalMilliseconds > 1000)
                {
                    changed = true;
                    Log("[tray] curator.mjs changed on disk; restarting the curator");
                    break;
                }
            }
            if (!p.HasExited)
            {
                // Closing stdin asks the curator to stop: it finishes a commit in progress, or abandons a model call.
                try { p.StandardInput.Close(); } catch { }
                if (!p.WaitForExit(15000))
                {
                    Log("[tray] curator did not stop within 15 s; killing pid " + p.Id);
                    try { p.Kill(); } catch { }
                    p.WaitForExit(5000);
                }
            }
            double lived = (DateTime.UtcNow - started).TotalSeconds;
            current = null;
            if (stopping) break;
            if (changed || restartRequested) { backoff = 1000; continue; }
            Log("[tray] curator exited with code " + p.ExitCode + " after " + (int)lived + "s; restarting in " + backoff + "ms");
            if (stopEvent.WaitOne(backoff) && stopping) break;
            backoff = lived > 60 ? 1000 : Math.Min(backoff * 2, 60000);
        }
        Log("[tray] curator host stopped");
        lock (logLock) { if (log != null) { log.Dispose(); log = null; } }
    }
}
