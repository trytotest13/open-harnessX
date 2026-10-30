// Bridge for launcher builds that always pass a fixed -c: run the real
// engine (llama-server-core.exe) without that flag so llama.cpp uses the
// selected model's own window, fitted to this device by --fit.
//
// The launcher still writes its configured ctxSize to /api/status and to the
// dsh patch; once the engine is healthy this wrapper copies the real n_ctx
// into the dsh patch so the IDE agent sees the same window. Disable the whole
// behaviour with "autoCtx": false in config.json. Drop this file and rename
// the core back once open-harness.exe implements autoCtx itself.
//
// Build with the .NET Framework compiler that ships with Windows:
//   %WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe /nologo /target:exe
//     /platform:x64 /out:engine\llama-server.exe /r:System.Web.Extensions.dll
//     engine-wrapper.cs

using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Web.Script.Serialization;

static class EngineWrapper
{
    const uint CREATE_NO_WINDOW = 0x08000000;
    const uint STARTF_USESTDHANDLES = 0x00000100;
    const uint PROCESS_SET_QUOTA = 0x00000100;
    const uint QUOTA_LIMITS_HARD_WS_MAX_ENABLE = 0x00000002;
    const uint INFINITE = 0xFFFFFFFF;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION
    {
        public IntPtr hProcess, hThread;
        public int dwProcessId, dwThreadId;
    }

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcess(string app, StringBuilder cmd, IntPtr pa, IntPtr ta,
        bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr h, uint ms);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr h, out uint code);

    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);

    [DllImport("kernel32.dll")]
    static extern IntPtr GetStdHandle(int which);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, int pid);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetProcessWorkingSetSizeEx(IntPtr h, IntPtr min, IntPtr max, uint flags);

    sealed class Config
    {
        public bool AutoCtx = true;
        public int? EnginePort;
        public double? RamLimitGB;
    }

    static int Main(string[] args)
    {
        string dir = AppDomain.CurrentDomain.BaseDirectory;
        string core = Path.Combine(dir, "llama-server-core.exe");
        Config cfg = ReadConfig(dir);
        bool stripCtx = cfg.AutoCtx && IsMainEngine(args, cfg);

        var kept = new List<string>();
        for (int i = 0; i < args.Length; i++)
        {
            string a = args[i];
            if (stripCtx && (a == "-c" || a == "--ctx-size") && i + 1 < args.Length) { i++; continue; }
            if (stripCtx && (a.StartsWith("--ctx-size=") || a.StartsWith("-c="))) continue;
            kept.Add(a);
        }

        var cmdline = new StringBuilder(Quote(core));
        foreach (string a in kept) cmdline.Append(' ').Append(Quote(a));

        var si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        si.dwFlags = (int)STARTF_USESTDHANDLES;
        si.hStdInput = GetStdHandle(-10);
        si.hStdOutput = GetStdHandle(-11);
        si.hStdError = GetStdHandle(-12);
        PROCESS_INFORMATION pi;
        if (!CreateProcess(core, cmdline, IntPtr.Zero, IntPtr.Zero, true,
                CREATE_NO_WINDOW, IntPtr.Zero, null, ref si, out pi))
        {
            Console.Error.WriteLine("[engine] cannot start " + core + " (win32 error "
                + Marshal.GetLastWin32Error() + ")");
            return 111;
        }
        CloseHandle(pi.hThread);
        try
        {
            ApplyRamCap(cfg.RamLimitGB, pi.dwProcessId);
            if (stripCtx)
            {
                Console.Error.WriteLine("[engine] auto context: letting llama.cpp size -c per model");
                StartContextSync(kept);
            }
            WaitForSingleObject(pi.hProcess, INFINITE);
            uint code;
            GetExitCodeProcess(pi.hProcess, out code);
            return unchecked((int)code);
        }
        finally { CloseHandle(pi.hProcess); }
    }

    /// Only the main engine is re-shaped: the OCR sidecar runs on its own
    /// port with a fixed 8192 context and must keep it.
    static bool IsMainEngine(string[] args, Config cfg)
    {
        string port = ArgValue(args, "--port");
        int p;
        return port != null && cfg.EnginePort != null
            && int.TryParse(port, out p) && p == cfg.EnginePort.Value;
    }

    static string ArgValue(IEnumerable<string> args, string name)
    {
        bool want = false;
        foreach (string a in args)
        {
            if (want) return a;
            if (a == name) want = true;
            else if (a.StartsWith(name + "=")) return a.Substring(name.Length + 1);
        }
        return null;
    }

    static Config ReadConfig(string engineDir)
    {
        var cfg = new Config();
        try
        {
            string root = Path.GetFullPath(Path.Combine(engineDir, ".."));
            string path = Path.Combine(root, "config.json");
            if (!File.Exists(path)) return cfg;
            var doc = new JavaScriptSerializer().DeserializeObject(File.ReadAllText(path))
                as Dictionary<string, object>;
            if (doc == null) return cfg;
            object v;
            if (doc.TryGetValue("ramLimitGB", out v)) cfg.RamLimitGB = ToDouble(v);
            object engObj;
            if (doc.TryGetValue("engine", out engObj))
            {
                var eng = engObj as Dictionary<string, object>;
                if (eng == null) return cfg;
                if (eng.TryGetValue("autoCtx", out v) && v is bool) cfg.AutoCtx = (bool)v;
                if (eng.TryGetValue("port", out v))
                {
                    double d = ToDouble(v);
                    if (d > 0) cfg.EnginePort = (int)d;
                }
            }
        }
        catch { }
        return cfg;
    }

    static double ToDouble(object v)
    {
        try { return v == null ? 0 : Convert.ToDouble(v); }
        catch { return 0; }
    }

    static void ApplyRamCap(double? gb, int pid)
    {
        if (gb == null || gb.Value <= 0) return;
        long max = (long)(gb.Value * 1024.0 * 1024.0 * 1024.0);
        IntPtr h = OpenProcess(PROCESS_SET_QUOTA, false, pid);
        if (h == IntPtr.Zero) return;
        try { SetProcessWorkingSetSizeEx(h, (IntPtr)(20L * 1024 * 1024), (IntPtr)max, QUOTA_LIMITS_HARD_WS_MAX_ENABLE); }
        finally { CloseHandle(h); }
    }

    /// Watch /props until the engine answers, then point the dsh patch at the
    /// window that actually got loaded (the launcher wrote its configured
    /// fallback there a moment earlier, so land after it).
    static void StartContextSync(List<string> args)
    {
        string port = ArgValue(args, "--port");
        string key = ArgValue(args, "--api-key");
        if (port == null) return;
        if (!string.IsNullOrEmpty(key))
        {
            int comma = key.IndexOf(',');
            if (comma > 0) key = key.Substring(0, comma);
        }
        var t = new Thread(() =>
        {
            int n = WaitForContext(port, key, TimeSpan.FromMinutes(4));
            if (n <= 0) return;
            Thread.Sleep(3000);
            try { PatchDsh(n); } catch { }
        });
        t.IsBackground = true;
        t.Start();
    }

    static int WaitForContext(string port, string key, TimeSpan budget)
    {
        string url = "http://127.0.0.1:" + port + "/props";
        DateTime deadline = DateTime.UtcNow + budget;
        while (DateTime.UtcNow < deadline)
        {
            try
            {
                var req = (HttpWebRequest)WebRequest.Create(url);
                req.Timeout = 2000;
                req.Proxy = null;
                req.KeepAlive = false;
                if (!string.IsNullOrEmpty(key)) req.Headers["Authorization"] = "Bearer " + key;
                using (var resp = (HttpWebResponse)req.GetResponse())
                using (var reader = new StreamReader(resp.GetResponseStream()))
                {
                    var doc = new JavaScriptSerializer().DeserializeObject(reader.ReadToEnd())
                        as Dictionary<string, object>;
                    int n = FindNCtx(doc);
                    if (n > 0) return n;
                }
            }
            catch { }
            Thread.Sleep(500);
        }
        return 0;
    }

    static int FindNCtx(Dictionary<string, object> doc)
    {
        if (doc == null) return 0;
        object v;
        if (doc.TryGetValue("default_generation_settings", out v))
        {
            var d = v as Dictionary<string, object>;
            if (d != null && d.TryGetValue("n_ctx", out v))
            {
                double n = ToDouble(v);
                if (n > 0) return (int)n;
            }
        }
        if (doc.TryGetValue("n_ctx", out v))
        {
            double n = ToDouble(v);
            if (n > 0) return (int)n;
        }
        return 0;
    }

    static void PatchDsh(int n)
    {
        string home = Environment.GetEnvironmentVariable("DSH_HOME");
        if (string.IsNullOrEmpty(home))
        {
            string user = Environment.GetEnvironmentVariable("USERPROFILE");
            if (string.IsNullOrEmpty(user)) return;
            home = Path.Combine(user, ".dsh");
        }
        string path = Path.Combine(Path.Combine(Path.Combine(home, "profiles"), "web"), "cordis.patch.yml");
        if (!File.Exists(path)) return;
        string text = File.ReadAllText(path);
        int at = text.IndexOf("- id: llm-pi-ai", StringComparison.Ordinal);
        if (at < 0) return;
        int end = text.IndexOf("\n- id: ", at + 1, StringComparison.Ordinal);
        if (end < 0) end = text.Length;
        string block = text.Substring(at, end - at);
        string fixedBlock = Regex.Replace(block, @"(contextWindow:\s*)\d+", "${1}" + n);
        fixedBlock = Regex.Replace(fixedBlock, @"(maxTokens:\s*)\d+", "${1}" + (n / 2));
        if (fixedBlock != block)
            File.WriteAllText(path, text.Substring(0, at) + fixedBlock + text.Substring(end),
                new UTF8Encoding(false));
    }

    static string Quote(string s)
    {
        if (s.Length > 0 && s.IndexOfAny(new char[] { ' ', '\t', '"' }) < 0) return s;
        var sb = new StringBuilder("\"");
        int backslashes = 0;
        foreach (char ch in s)
        {
            if (ch == '\\') { sb.Append(ch); backslashes++; continue; }
            if (ch == '"') { sb.Append('\\', backslashes + 1).Append('"'); backslashes = 0; continue; }
            sb.Append(ch);
            backslashes = 0;
        }
        sb.Append('\\', backslashes).Append('"');
        return sb.ToString();
    }
}
