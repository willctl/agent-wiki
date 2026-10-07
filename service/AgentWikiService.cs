// AgentWikiService: runs the agent-wiki MCP server (`node server.mjs --http`) as a
// Windows service. It restarts the server when it exits (with backoff) and when
// server.mjs changes on disk, so `npm run install-local` upgrades take effect
// without admin rights. Settings come from AgentWikiService.ini next to the exe.
//
// C# 5, built at install time by the .NET Framework csc.exe that ships with
// Windows (no third-party service wrapper). `--console` runs the same loop in
// the foreground for testing; it stops when stdin closes.

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.ServiceProcess;
using System.Text;
using System.Threading;

public class AgentWikiService : ServiceBase
{
    const long MaxLogBytes = 5 * 1024 * 1024;
    readonly Dictionary<string, string> cfg;
    readonly object logLock = new object();
    readonly ManualResetEvent stopRequested = new ManualResetEvent(false);
    readonly ManualResetEvent stopped = new ManualResetEvent(false);
    volatile bool stopping;
    Thread worker;
    StreamWriter log;
    bool echo;

    public AgentWikiService(Dictionary<string, string> cfg)
    {
        this.cfg = cfg;
        ServiceName = "AgentWiki";
        CanStop = true;
        CanShutdown = true;
        AutoLog = false;
    }

    public static int Main(string[] args)
    {
        string ini = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "AgentWikiService.ini");
        for (int i = 0; i < args.Length - 1; i++)
        {
            if (args[i] == "--config") ini = args[i + 1];
        }
        Dictionary<string, string> cfg;
        try
        {
            cfg = ReadIni(ini);
            foreach (string k in new[] { "node", "script", "wikiDir", "port", "logDir" })
            {
                if (!cfg.ContainsKey(k)) throw new Exception("missing key '" + k + "'");
            }
        }
        catch (Exception e)
        {
            Console.Error.WriteLine("AgentWikiService: cannot use " + ini + ": " + e.Message);
            return 2;
        }

        AgentWikiService svc = new AgentWikiService(cfg);
        if (Array.IndexOf(args, "--console") < 0)
        {
            ServiceBase.Run(svc);
            return 0;
        }
        svc.echo = true;
        svc.StartWorker();
        Console.CancelKeyPress += delegate(object s, ConsoleCancelEventArgs e) { e.Cancel = true; svc.RequestStop(); };
        Thread stdinWatch = new Thread(delegate()
        {
            try { while (Console.In.ReadLine() != null) { } } catch { }
            svc.RequestStop();
        });
        stdinWatch.IsBackground = true;
        stdinWatch.Start();
        svc.stopped.WaitOne();
        return 0;
    }

    static Dictionary<string, string> ReadIni(string path)
    {
        Dictionary<string, string> d = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (string raw in File.ReadAllLines(path, Encoding.UTF8))
        {
            string line = raw.Trim();
            if (line.Length == 0 || line.StartsWith("#") || line.StartsWith(";")) continue;
            int eq = line.IndexOf('=');
            if (eq > 0) d[line.Substring(0, eq).Trim()] = line.Substring(eq + 1).Trim();
        }
        return d;
    }

    protected override void OnStart(string[] args) { StartWorker(); }
    protected override void OnStop() { RequestStop(); stopped.WaitOne(20000); }
    protected override void OnShutdown() { RequestStop(); stopped.WaitOne(10000); }

    void StartWorker()
    {
        Directory.CreateDirectory(cfg["logDir"]);
        worker = new Thread(Loop);
        worker.IsBackground = true;
        worker.Start();
    }

    void RequestStop()
    {
        stopping = true;
        stopRequested.Set();
    }

    string LogPath { get { return Path.Combine(cfg["logDir"], "service.log"); } }

    void OpenLog()
    {
        lock (logLock)
        {
            if (log != null) { log.Dispose(); log = null; }
            try
            {
                FileInfo fi = new FileInfo(LogPath);
                if (fi.Exists && fi.Length > MaxLogBytes)
                {
                    string old = LogPath + ".1";
                    if (File.Exists(old)) File.Delete(old);
                    File.Move(LogPath, old);
                }
            }
            catch { }
            FileStream fs = new FileStream(LogPath, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete);
            log = new StreamWriter(fs, new UTF8Encoding(false));
            log.NewLine = "\n";
            log.AutoFlush = true;
        }
    }

    void Log(string msg)
    {
        if (msg == null) return;
        string line = DateTime.Now.ToString("yyyy-MM-ddTHH:mm:ss.fffzzz") + " " + msg;
        lock (logLock)
        {
            try { if (log != null) log.WriteLine(line); } catch { }
            if (echo) Console.WriteLine(line);
        }
    }

    static string Quote(string s) { return "\"" + s.Replace("\"", "\\\"") + "\""; }

    static DateTime ScriptStamp(string script)
    {
        try { return File.GetLastWriteTimeUtc(script); } catch { return DateTime.MinValue; }
    }

    void Loop()
    {
        int backoffMs = 1000;
        try
        {
            while (!stopping)
            {
                OpenLog();
                string script = cfg["script"];
                DateTime stamp = ScriptStamp(script);
                Process p = new Process();
                p.StartInfo.FileName = cfg["node"];
                // --preserve-symlinks-main: Node otherwise resolves the script's real path by checking every folder on
                // the way (lstat), and this account may not read the attributes of the user's AppData folder.
                p.StartInfo.Arguments = "--preserve-symlinks-main " + Quote(script) + " --http --port " + cfg["port"] + " --parent-stdin";
                p.StartInfo.WorkingDirectory = Path.GetDirectoryName(script);
                p.StartInfo.UseShellExecute = false;
                p.StartInfo.CreateNoWindow = true;
                p.StartInfo.RedirectStandardInput = true;
                p.StartInfo.RedirectStandardOutput = true;
                p.StartInfo.RedirectStandardError = true;
                p.StartInfo.EnvironmentVariables["AGENT_WIKI_DIR"] = cfg["wikiDir"];
                // Where the server finds config.json and writes its logs: this account's profile is not
                // the user's, so the installer names each folder (src/paths.mjs). "home" is the pre-1.3 layout.
                p.StartInfo.EnvironmentVariables["AGENT_WIKI_LOG_DIR"] = cfg["logDir"];
                foreach (string[] kv in new[] { new[] { "configDir", "AGENT_WIKI_CONFIG_DIR" }, new[] { "dataDir", "AGENT_WIKI_DATA_DIR" }, new[] { "stateDir", "AGENT_WIKI_STATE_DIR" }, new[] { "cacheDir", "AGENT_WIKI_CACHE_DIR" }, new[] { "home", "AGENT_WIKI_HOME" } })
                {
                    if (cfg.ContainsKey(kv[0])) p.StartInfo.EnvironmentVariables[kv[1]] = cfg[kv[0]];
                }
                p.OutputDataReceived += delegate(object s, DataReceivedEventArgs e) { Log(e.Data); };
                p.ErrorDataReceived += delegate(object s, DataReceivedEventArgs e) { Log(e.Data); };
                DateTime started = DateTime.UtcNow;
                try
                {
                    p.Start();
                }
                catch (Exception e)
                {
                    Log("[service] cannot start " + cfg["node"] + ": " + e.Message);
                    if (stopRequested.WaitOne(30000)) break;
                    continue;
                }
                p.BeginOutputReadLine();
                p.BeginErrorReadLine();
                Log("[service] started server pid " + p.Id + ": " + cfg["node"] + " " + p.StartInfo.Arguments);

                bool changed = false;
                while (!p.HasExited && !stopping)
                {
                    if (stopRequested.WaitOne(1000)) break;
                    DateTime now = ScriptStamp(script);
                    // Restart once the new file has been stable for a second.
                    if (now != stamp && now != DateTime.MinValue && (DateTime.UtcNow - now).TotalMilliseconds > 1000)
                    {
                        changed = true;
                        Log("[service] server.mjs changed on disk; restarting the server");
                        break;
                    }
                }

                if (!p.HasExited) StopChild(p);
                double lived = (DateTime.UtcNow - started).TotalSeconds;
                if (stopping) break;
                if (changed) { backoffMs = 1000; continue; }

                Log("[service] server exited with code " + p.ExitCode + " after " + (int)lived + "s; restarting in " + backoffMs + "ms");
                if (stopRequested.WaitOne(backoffMs)) break;
                backoffMs = lived > 60 ? 1000 : Math.Min(backoffMs * 2, 30000);
            }
        }
        catch (Exception e)
        {
            Log("[service] fatal: " + e);
        }
        finally
        {
            Log("[service] stopped");
            lock (logLock) { if (log != null) { log.Dispose(); log = null; } }
            stopped.Set();
        }
    }

    void StopChild(Process p)
    {
        // Closing stdin asks the server to finish in-flight writes and exit.
        try { p.StandardInput.Close(); } catch { }
        if (!p.WaitForExit(5000))
        {
            Log("[service] server did not exit within 5s; killing pid " + p.Id);
            try { p.Kill(); } catch { }
            p.WaitForExit(5000);
        }
    }
}
