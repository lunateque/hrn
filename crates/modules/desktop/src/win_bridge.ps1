$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName WindowsBase

Add-Type -ReferencedAssemblies System.Drawing, System.Windows.Forms, UIAutomationClient, UIAutomationTypes, WindowsBase -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Automation.Text;
using System.Windows.Forms;

public class HrnDesktopException : Exception {
    public string Code { get; private set; }
    public HrnDesktopException(string code, string message) : base(message) {
        Code = code;
    }
}

public static class HrnWinDesktop {
    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr dpiContext);
    [DllImport("user32.dll")] static extern bool SetCursorPos(int X, int Y);
    [DllImport("user32.dll")] static extern void mouse_event(uint dwFlags, int dx, int dy, int dwData, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] static extern bool IsHungAppWindow(IntPtr hWnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int maxCount);
    [DllImport("user32.dll")] static extern int GetWindowTextLength(IntPtr hWnd);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hWnd, uint uCmd);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("user32.dll", SetLastError = true)] static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr SendMessage(IntPtr hWnd, uint Msg, IntPtr wParam, string lParam);
    [DllImport("user32.dll", EntryPoint = "SendMessage")] static extern IntPtr SendMessagePtr(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam, uint fuFlags, uint uTimeout, out IntPtr lpdwResult);
    [DllImport("user32.dll")] static extern bool ScreenToClient(IntPtr hWnd, ref POINT lpPoint);
    [DllImport("user32.dll")] static extern IntPtr ChildWindowFromPointEx(IntPtr hWndParent, POINT pt, uint uFlags);
    [DllImport("user32.dll")] static extern bool GetGUIThreadInfo(uint idThread, ref GUITHREADINFO lpgui);
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr OpenInputDesktop(uint dwFlags, bool fInherit, uint dwDesiredAccess);
    [DllImport("user32.dll", SetLastError = true)] static extern bool CloseDesktop(IntPtr hDesktop);
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool GetUserObjectInformation(IntPtr hObj, int nIndex, byte[] pvInfo, uint nLength, out uint lpnLengthNeeded);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr hwnd, int dwAttribute, out int pvAttribute, int cbAttribute);

    delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X, Y; }

    [StructLayout(LayoutKind.Sequential)]
    struct GUITHREADINFO {
        public int cbSize;
        public uint flags;
        public IntPtr hwndActive;
        public IntPtr hwndFocus;
        public IntPtr hwndCapture;
        public IntPtr hwndMenuOwner;
        public IntPtr hwndMoveSize;
        public IntPtr hwndCaret;
        public RECT rcCaret;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct INPUT { public uint type; public InputUnion u; }

    [StructLayout(LayoutKind.Explicit)]
    struct InputUnion {
        [FieldOffset(0)] public KEYBDINPUT ki;
        [FieldOffset(0)] public MOUSEINPUT mi;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }

    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }

    const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    const uint MOUSEEVENTF_LEFTUP = 0x0004;
    const uint MOUSEEVENTF_RIGHTDOWN = 0x0008;
    const uint MOUSEEVENTF_RIGHTUP = 0x0010;
    const uint MOUSEEVENTF_WHEEL = 0x0800;
    const uint KEYEVENTF_KEYUP = 0x0002;
    const uint KEYEVENTF_UNICODE = 0x0004;

    const uint GW_HWNDPREV = 3;
    const uint PW_RENDERFULLCONTENT = 2;
    const int DWMWA_CLOAKED = 14;
    const int SW_RESTORE = 9;
    const uint DESKTOP_READOBJECTS = 0x0001;
    const int UOI_NAME = 2;
    const uint CWP_SKIPINVISIBLE = 0x0001;
    const uint SMTO_ABORTIFHUNG = 0x0002;

    const uint WM_NULL = 0x0000;
    const uint WM_SETTEXT = 0x000C;
    const uint EM_SETSEL = 0x00B1;
    const uint WM_KEYDOWN = 0x0100;
    const uint WM_KEYUP = 0x0101;
    const uint WM_CHAR = 0x0102;
    const uint WM_LBUTTONDOWN = 0x0201;
    const uint WM_LBUTTONUP = 0x0202;
    const uint WM_LBUTTONDBLCLK = 0x0203;
    const uint WM_RBUTTONDOWN = 0x0204;
    const uint WM_RBUTTONUP = 0x0205;
    const int MK_LBUTTON = 0x0001;
    const int MK_RBUTTON = 0x0002;

    public static void Init() {
        try {
            if (!SetProcessDpiAwarenessContext(new IntPtr(-4))) {
                SetProcessDPIAware();
            }
        } catch {
            SetProcessDPIAware();
        }
    }

    static bool IsWindowCloaked(IntPtr hWnd) {
        try {
            int cloaked = 0;
            int hr = DwmGetWindowAttribute(hWnd, DWMWA_CLOAKED, out cloaked, Marshal.SizeOf(typeof(int)));
            return hr == 0 && cloaked != 0;
        } catch {
            return false;
        }
    }

    public static bool IsScreenLocked() {
        IntPtr hDesk = OpenInputDesktop(0, false, DESKTOP_READOBJECTS);
        if (hDesk == IntPtr.Zero) return true;
        try {
            byte[] buf = new byte[256];
            uint needed = 0;
            if (GetUserObjectInformation(hDesk, UOI_NAME, buf, (uint)buf.Length, out needed) && needed >= 2) {
                string name = Encoding.Unicode.GetString(buf, 0, (int)needed).TrimEnd('\0');
                if (!string.Equals(name, "Default", StringComparison.OrdinalIgnoreCase)) return true;
            }
            return false;
        } finally {
            CloseDesktop(hDesk);
        }
    }

    static IntPtr ResolveWindow(long hwndVal, string windowTitle, bool defaultForeground) {
        if (hwndVal != 0) {
            IntPtr h = new IntPtr(hwndVal);
            if (!IsWindow(h)) {
                throw new HrnDesktopException("WindowNotFound", "window handle is no longer valid: " + hwndVal);
            }
            return h;
        }
        if (!string.IsNullOrEmpty(windowTitle)) {
            IntPtr match = FindWindowByTitle(windowTitle);
            if (match == IntPtr.Zero) {
                throw new HrnDesktopException("WindowNotFound", "window not found matching: " + windowTitle);
            }
            return match;
        }
        if (defaultForeground) {
            IntPtr fg = GetForegroundWindow();
            if (fg == IntPtr.Zero || !IsWindow(fg)) {
                throw new HrnDesktopException("WindowNotFound", "no active foreground window");
            }
            return fg;
        }
        return IntPtr.Zero;
    }

    static IntPtr FindWindowByTitle(string needle) {
        IntPtr match = IntPtr.Zero;
        EnumWindows(delegate(IntPtr hWnd, IntPtr lp) {
            if (!IsWindowVisible(hWnd) || IsWindowCloaked(hWnd)) return true;
            int len = GetWindowTextLength(hWnd);
            if (len <= 0) return true;
            StringBuilder sb = new StringBuilder(len + 1);
            GetWindowText(hWnd, sb, sb.Capacity);
            if (sb.ToString().IndexOf(needle, StringComparison.OrdinalIgnoreCase) >= 0) {
                match = hWnd;
                return false;
            }
            return true;
        }, IntPtr.Zero);
        return match;
    }

    static Rectangle ResolveDisplayBounds(bool allMonitors, string display) {
        if (allMonitors || string.Equals(display, "all", StringComparison.OrdinalIgnoreCase)) {
            return SystemInformation.VirtualScreen;
        }
        if (string.Equals(display, "primary", StringComparison.OrdinalIgnoreCase)) {
            return Screen.PrimaryScreen.Bounds;
        }
        if (string.IsNullOrEmpty(display) || string.Equals(display, "active", StringComparison.OrdinalIgnoreCase)) {
            IntPtr fg = GetForegroundWindow();
            if (fg != IntPtr.Zero && IsWindow(fg)) {
                return Screen.FromHandle(fg).Bounds;
            }
            return Screen.FromPoint(Cursor.Position).Bounds;
        }
        Screen[] screens = Screen.AllScreens;
        int idx;
        if (int.TryParse(display, out idx) && idx >= 0 && idx < screens.Length) {
            return screens[idx].Bounds;
        }
        for (int i = 0; i < screens.Length; i++) {
            if (screens[i].DeviceName.IndexOf(display, StringComparison.OrdinalIgnoreCase) >= 0) {
                return screens[i].Bounds;
            }
        }
        return Screen.PrimaryScreen.Bounds;
    }

    static string EncodeBitmapPng(Bitmap src, int maxWidth, int maxHeight) {
        Bitmap toSave = src;
        Bitmap resized = null;
        try {
            if (maxWidth > 0 && maxHeight > 0 && (src.Width > maxWidth || src.Height > maxHeight)) {
                double sx = (double)maxWidth / (double)src.Width;
                double sy = (double)maxHeight / (double)src.Height;
                double s = Math.Min(sx, sy);
                int nw = Math.Max(1, (int)Math.Round(src.Width * s));
                int nh = Math.Max(1, (int)Math.Round(src.Height * s));
                resized = new Bitmap(nw, nh, PixelFormat.Format32bppArgb);
                using (Graphics g = Graphics.FromImage(resized)) {
                    g.InterpolationMode = InterpolationMode.HighQualityBilinear;
                    g.DrawImage(src, 0, 0, nw, nh);
                }
                toSave = resized;
            }
            using (MemoryStream ms = new MemoryStream()) {
                toSave.Save(ms, ImageFormat.Png);
                return Convert.ToBase64String(ms.ToArray());
            }
        } finally {
            if (resized != null) resized.Dispose();
        }
    }

    static bool IsWindowCovered(IntPtr targetHwnd, RECT targetRect) {
        Rectangle tr = Rectangle.FromLTRB(targetRect.Left, targetRect.Top, targetRect.Right, targetRect.Bottom);
        IntPtr cur = GetWindow(targetHwnd, GW_HWNDPREV);
        int guard = 0;
        while (cur != IntPtr.Zero && guard++ < 512) {
            if (IsWindowVisible(cur) && !IsWindowCloaked(cur)) {
                int tlen = GetWindowTextLength(cur);
                if (tlen > 0) {
                    RECT cr;
                    if (GetWindowRect(cur, out cr)) {
                        int cw = cr.Right - cr.Left;
                        int ch = cr.Bottom - cr.Top;
                        if (cw > 10 && ch > 10) {
                            Rectangle r2 = Rectangle.FromLTRB(cr.Left, cr.Top, cr.Right, cr.Bottom);
                            r2.Intersect(tr);
                            if (!r2.IsEmpty && r2.Width > 0 && r2.Height > 0) {
                                return true;
                            }
                        }
                    }
                }
            }
            cur = GetWindow(cur, GW_HWNDPREV);
        }
        return false;
    }

    static Bitmap CaptureWindowBitmap(IntPtr targetHwnd, out RECT r, bool preferPrintWindow) {
        if (!GetWindowRect(targetHwnd, out r)) {
            throw new HrnDesktopException("CaptureFailed", "GetWindowRect failed");
        }
        int w = Math.Max(1, r.Right - r.Left);
        int h = Math.Max(1, r.Bottom - r.Top);
        Bitmap bmp = new Bitmap(w, h, PixelFormat.Format32bppArgb);
        try {
            using (Graphics g = Graphics.FromImage(bmp)) {
                if (preferPrintWindow) {
                    bool printed = false;
                    IntPtr hdc = g.GetHdc();
                    try {
                        printed = PrintWindow(targetHwnd, hdc, PW_RENDERFULLCONTENT);
                        if (!printed) printed = PrintWindow(targetHwnd, hdc, 0);
                    } finally {
                        g.ReleaseHdc(hdc);
                    }
                    if (!printed) {
                        g.CopyFromScreen(r.Left, r.Top, 0, 0, new System.Drawing.Size(w, h), CopyPixelOperation.SourceCopy);
                    }
                } else {
                    try {
                        g.CopyFromScreen(r.Left, r.Top, 0, 0, new System.Drawing.Size(w, h), CopyPixelOperation.SourceCopy);
                    } catch (System.ComponentModel.Win32Exception ex) {
                        bool printed = false;
                        IntPtr hdc = g.GetHdc();
                        try {
                            printed = PrintWindow(targetHwnd, hdc, PW_RENDERFULLCONTENT);
                        } finally {
                            g.ReleaseHdc(hdc);
                        }
                        if (!printed) {
                            string code = IsScreenLocked() ? "ScreenLocked" : "CaptureFailed";
                            throw new HrnDesktopException(code, "screen capture unavailable (desktop may be locked, UAC-prompted, or non-interactive): " + ex.Message);
                        }
                    }
                }
            }
            return bmp;
        } catch {
            bmp.Dispose();
            throw;
        }
    }

    public static object Capture(bool allMonitors, string windowTitle, long hwndVal, string display, int maxWidth, int maxHeight) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, false);
        if (targetHwnd != IntPtr.Zero) {
            RECT r;
            using (Bitmap bmp = CaptureWindowBitmap(targetHwnd, out r, false)) {
                int w = Math.Max(1, r.Right - r.Left);
                int h = Math.Max(1, r.Bottom - r.Top);
                return new {
                    ok = true,
                    hwnd = targetHwnd.ToInt64(),
                    origin_x = r.Left,
                    origin_y = r.Top,
                    width = w,
                    height = h,
                    png_b64 = EncodeBitmapPng(bmp, maxWidth, maxHeight)
                };
            }
        }
        Rectangle bounds = ResolveDisplayBounds(allMonitors, display);
        using (Bitmap bmp = new Bitmap(Math.Max(1, bounds.Width), Math.Max(1, bounds.Height), PixelFormat.Format32bppArgb)) {
            using (Graphics g = Graphics.FromImage(bmp)) {
                try {
                    g.CopyFromScreen(bounds.Left, bounds.Top, 0, 0, bounds.Size, CopyPixelOperation.SourceCopy);
                } catch (System.ComponentModel.Win32Exception ex) {
                    string code = IsScreenLocked() ? "ScreenLocked" : "CaptureFailed";
                    throw new HrnDesktopException(code, "screen capture unavailable (desktop may be locked, UAC-prompted, or non-interactive): " + ex.Message);
                }
            }
            return new {
                ok = true,
                origin_x = bounds.Left,
                origin_y = bounds.Top,
                width = bounds.Width,
                height = bounds.Height,
                png_b64 = EncodeBitmapPng(bmp, maxWidth, maxHeight)
            };
        }
    }

    public static object WindowCapture(long hwndVal, string windowTitle, int maxWidth, int maxHeight) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, true);
        RECT r;
        using (Bitmap bmp = CaptureWindowBitmap(targetHwnd, out r, true)) {
            int w = Math.Max(1, r.Right - r.Left);
            int h = Math.Max(1, r.Bottom - r.Top);
            bool covered = IsWindowCovered(targetHwnd, r);
            return new {
                ok = true,
                hwnd = targetHwnd.ToInt64(),
                origin_x = r.Left,
                origin_y = r.Top,
                width = w,
                height = h,
                covered = covered,
                png_b64 = EncodeBitmapPng(bmp, maxWidth, maxHeight)
            };
        }
    }

    public static object WindowRect(long hwndVal) {
        IntPtr h = ResolveWindow(hwndVal, "", false);
        RECT r;
        if (!GetWindowRect(h, out r)) {
            throw new HrnDesktopException("WindowNotFound", "GetWindowRect failed for HWND " + hwndVal);
        }
        return new {
            ok = true,
            hwnd = h.ToInt64(),
            x = r.Left,
            y = r.Top,
            w = Math.Max(1, r.Right - r.Left),
            h = Math.Max(1, r.Bottom - r.Top)
        };
    }

    public static object Zoom(int x, int y, int w, int h, long hwndVal, string windowTitle, int maxWidth, int maxHeight) {
        int cw = Math.Max(1, w);
        int ch = Math.Max(1, h);
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, false);
        if (targetHwnd != IntPtr.Zero) {
            RECT r;
            using (Bitmap full = CaptureWindowBitmap(targetHwnd, out r, true)) {
                int lx = Math.Max(0, Math.Min(full.Width - 1, x - r.Left));
                int ly = Math.Max(0, Math.Min(full.Height - 1, y - r.Top));
                int lw = Math.Max(1, Math.Min(cw, full.Width - lx));
                int lh = Math.Max(1, Math.Min(ch, full.Height - ly));
                using (Bitmap cropped = full.Clone(new Rectangle(lx, ly, lw, lh), PixelFormat.Format32bppArgb)) {
                    return new {
                        ok = true,
                        x = r.Left + lx,
                        y = r.Top + ly,
                        width = lw,
                        height = lh,
                        png_b64 = EncodeBitmapPng(cropped, maxWidth, maxHeight)
                    };
                }
            }
        }
        using (Bitmap bmp = new Bitmap(cw, ch, PixelFormat.Format32bppArgb)) {
            using (Graphics g = Graphics.FromImage(bmp)) {
                try {
                    g.CopyFromScreen(x, y, 0, 0, new System.Drawing.Size(cw, ch), CopyPixelOperation.SourceCopy);
                } catch (System.ComponentModel.Win32Exception ex) {
                    string code = IsScreenLocked() ? "ScreenLocked" : "CaptureFailed";
                    throw new HrnDesktopException(code, "screen capture unavailable: " + ex.Message);
                }
            }
            return new {
                ok = true,
                x = x,
                y = y,
                width = cw,
                height = ch,
                png_b64 = EncodeBitmapPng(bmp, maxWidth, maxHeight)
            };
        }
    }

    public static object ListWindows() {
        List<object> list = new List<object>();
        EnumWindows(delegate(IntPtr hWnd, IntPtr lp) {
            if (!IsWindowVisible(hWnd) || IsWindowCloaked(hWnd)) return true;
            int len = GetWindowTextLength(hWnd);
            if (len <= 0) return true;
            StringBuilder sb = new StringBuilder(len + 1);
            GetWindowText(hWnd, sb, sb.Capacity);
            RECT r;
            if (!GetWindowRect(hWnd, out r)) return true;
            int w = r.Right - r.Left;
            int h = r.Bottom - r.Top;
            if (w <= 10 || h <= 10) return true;
            uint pid;
            GetWindowThreadProcessId(hWnd, out pid);
            list.Add(new { hwnd = hWnd.ToInt64(), title = sb.ToString(), x = r.Left, y = r.Top, w = w, h = h, pid = pid });
            return true;
        }, IntPtr.Zero);
        return new { ok = true, windows = list };
    }

    public static object Displays() {
        Screen[] screens = Screen.AllScreens;
        List<object> list = new List<object>();
        for (int i = 0; i < screens.Length; i++) {
            Screen s = screens[i];
            list.Add(new {
                id = i,
                name = s.DeviceName,
                primary = s.Primary,
                x = s.Bounds.X,
                y = s.Bounds.Y,
                width = s.Bounds.Width,
                height = s.Bounds.Height
            });
        }
        return new { ok = true, displays = list };
    }

    public static object Capabilities() {
        bool locked = IsScreenLocked();
        return new {
            ok = true,
            screenLocked = locked,
            uia = true,
            backgroundInput = true,
            os = "windows"
        };
    }

    public static object AppsList() {
        Dictionary<uint, int> counts = new Dictionary<uint, int>();
        EnumWindows(delegate(IntPtr hWnd, IntPtr lp) {
            if (!IsWindowVisible(hWnd) || IsWindowCloaked(hWnd)) return true;
            if (GetWindowTextLength(hWnd) <= 0) return true;
            RECT r;
            if (!GetWindowRect(hWnd, out r)) return true;
            if ((r.Right - r.Left) <= 10 || (r.Bottom - r.Top) <= 10) return true;
            uint pid;
            GetWindowThreadProcessId(hWnd, out pid);
            if (counts.ContainsKey(pid)) counts[pid] = counts[pid] + 1;
            else counts[pid] = 1;
            return true;
        }, IntPtr.Zero);

        List<object> apps = new List<object>();
        foreach (KeyValuePair<uint, int> kv in counts) {
            string name = "pid-" + kv.Key;
            try {
                using (Process p = Process.GetProcessById((int)kv.Key)) {
                    name = p.ProcessName;
                }
            } catch { }
            apps.Add(new { name = name, pid = kv.Key, window_count = kv.Value });
        }
        return new { ok = true, apps = apps };
    }

    public static object OpenApp(string target, string[] args) {
        if (string.IsNullOrWhiteSpace(target)) {
            throw new HrnDesktopException("AxFailed", "open_app requires target");
        }
        ProcessStartInfo psi = new ProcessStartInfo();
        psi.FileName = target;
        psi.UseShellExecute = true;
        if (args != null && args.Length > 0) {
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < args.Length; i++) {
                if (i > 0) sb.Append(' ');
                string a = args[i] ?? "";
                if (a.IndexOf(' ') >= 0 || a.IndexOf('"') >= 0) {
                    sb.Append('"').Append(a.Replace("\"", "\\\"")).Append('"');
                } else {
                    sb.Append(a);
                }
            }
            psi.Arguments = sb.ToString();
        }
        Process proc = Process.Start(psi);
        int pid = 0;
        if (proc != null) {
            try { pid = proc.Id; } catch { }
        }
        return new { ok = true, pid = pid, target = target };
    }

    public static object ClipboardRead() {
        string text = "";
        bool hasText = false;
        Exception err = null;
        Thread t = new Thread(delegate() {
            try {
                if (Clipboard.ContainsText()) {
                    text = Clipboard.GetText();
                    hasText = true;
                }
            } catch (Exception ex) { err = ex; }
        });
        t.SetApartmentState(ApartmentState.STA);
        t.Start();
        if (!t.Join(4000)) throw new HrnDesktopException("InputBusy", "clipboard read timed out");
        if (err != null) throw new HrnDesktopException("AxFailed", "clipboard read failed: " + err.Message);
        return new { ok = true, text = text, has_text = hasText };
    }

    public static object ClipboardWrite(string text) {
        Exception err = null;
        string val = text ?? "";
        Thread t = new Thread(delegate() {
            try {
                if (val.Length == 0) Clipboard.Clear();
                else Clipboard.SetText(val);
            } catch (Exception ex) { err = ex; }
        });
        t.SetApartmentState(ApartmentState.STA);
        t.Start();
        if (!t.Join(4000)) throw new HrnDesktopException("InputBusy", "clipboard write timed out");
        if (err != null) throw new HrnDesktopException("AxFailed", "clipboard write failed: " + err.Message);
        return new { ok = true, chars = val.Length };
    }

    static string GetRoleName(AutomationElement el) {
        try {
            string p = el.Current.ControlType.ProgrammaticName;
            if (!string.IsNullOrEmpty(p) && p.StartsWith("ControlType.", StringComparison.OrdinalIgnoreCase)) {
                return p.Substring("ControlType.".Length);
            }
            return string.IsNullOrEmpty(p) ? "Unknown" : p;
        } catch {
            return "Unknown";
        }
    }

    static string NormalizeRole(string raw) {
        if (string.IsNullOrEmpty(raw)) return "";
        string s = raw.Trim().ToLowerInvariant();
        if (s.StartsWith("controltype.")) s = s.Substring("controltype.".Length);
        StringBuilder sb = new StringBuilder(s.Length);
        for (int i = 0; i < s.Length; i++) {
            char c = s[i];
            if (c != '_' && c != '-' && c != ' ') sb.Append(c);
        }
        string k = sb.ToString();
        if (k == "textbox" || k == "textfield" || k == "textarea" || k == "input" || k == "edit") return "edit";
        if (k == "button" || k == "pushbutton" || k == "btn") return "button";
        if (k == "checkbox" || k == "check") return "checkbox";
        if (k == "radiobutton" || k == "radio") return "radiobutton";
        if (k == "combobox" || k == "dropdown" || k == "select") return "combobox";
        if (k == "hyperlink" || k == "link") return "hyperlink";
        if (k == "text" || k == "statictext" || k == "label") return "text";
        if (k == "tab" || k == "tablist") return "tab";
        if (k == "image" || k == "img") return "image";
        return k;
    }

    static string ReadElementValue(AutomationElement el, int maxChars) {
        try {
            object pat;
            if (el.TryGetCurrentPattern(ValuePattern.Pattern, out pat)) {
                string v = ((ValuePattern)pat).Current.Value;
                if (v != null) {
                    if (maxChars > 0 && v.Length > maxChars) return v.Substring(0, maxChars);
                    return v;
                }
            }
            if (el.TryGetCurrentPattern(RangeValuePattern.Pattern, out pat)) {
                return ((RangeValuePattern)pat).Current.Value.ToString();
            }
            string role = NormalizeRole(GetRoleName(el));
            if ((role == "edit" || role == "document") && el.TryGetCurrentPattern(TextPattern.Pattern, out pat)) {
                TextPattern tp = (TextPattern)pat;
                string t = tp.DocumentRange.GetText(maxChars > 0 ? maxChars : -1);
                return t ?? "";
            }
        } catch { }
        return "";
    }

    static List<string> ReadElementStates(AutomationElement el) {
        List<string> states = new List<string>();
        try {
            if (!el.Current.IsEnabled) states.Add("disabled");
            if (el.Current.HasKeyboardFocus) states.Add("focused");
            if (el.Current.IsOffscreen) states.Add("offscreen");
            object pat;
            if (el.TryGetCurrentPattern(TogglePattern.Pattern, out pat)) {
                ToggleState ts = ((TogglePattern)pat).Current.ToggleState;
                if (ts == ToggleState.On) states.Add("checked");
                else if (ts == ToggleState.Indeterminate) states.Add("indeterminate");
            }
            if (el.TryGetCurrentPattern(ExpandCollapsePattern.Pattern, out pat)) {
                ExpandCollapseState es = ((ExpandCollapsePattern)pat).Current.ExpandCollapseState;
                if (es == ExpandCollapseState.Expanded) states.Add("expanded");
                else if (es == ExpandCollapseState.Collapsed) states.Add("collapsed");
            }
            if (el.TryGetCurrentPattern(SelectionItemPattern.Pattern, out pat)) {
                if (((SelectionItemPattern)pat).Current.IsSelected) states.Add("selected");
            }
            if (el.TryGetCurrentPattern(ValuePattern.Pattern, out pat)) {
                if (((ValuePattern)pat).Current.IsReadOnly) states.Add("readonly");
            }
        } catch { }
        return states;
    }

    static object SerializeElement(AutomationElement el, int depth) {
        string role = GetRoleName(el);
        string name = "";
        try { name = el.Current.Name ?? ""; } catch { }
        string val = ReadElementValue(el, 200);
        List<string> states = ReadElementStates(el);
        int[] rid = new int[0];
        try {
            int[] got = el.GetRuntimeId();
            if (got != null) rid = got;
        } catch { }
        int x = 0, y = 0, w = 0, h = 0;
        try {
            Rect br = el.Current.BoundingRectangle;
            if (!br.IsEmpty && !double.IsInfinity(br.X) && !double.IsNaN(br.X)) {
                x = (int)Math.Round(br.X);
                y = (int)Math.Round(br.Y);
                w = Math.Max(0, (int)Math.Round(br.Width));
                h = Math.Max(0, (int)Math.Round(br.Height));
            }
        } catch { }
        return new {
            role = role,
            name = name,
            value = val,
            states = states,
            runtime_id = rid,
            x = x,
            y = y,
            w = w,
            h = h,
            depth = depth
        };
    }

    public static object AxTree(long hwndVal, string windowTitle, int maxDepth, int maxNodes) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, true);
        int depthLimit = maxDepth > 0 ? maxDepth : 24;
        int nodeLimit = (maxNodes > 0 && maxNodes <= 800) ? maxNodes : 800;

        AutomationElement root;
        try {
            root = AutomationElement.FromHandle(targetHwnd);
        } catch (Exception ex) {
            throw new HrnDesktopException("AxFailed", "AutomationElement.FromHandle failed: " + ex.Message);
        }
        if (root == null) throw new HrnDesktopException("AxFailed", "UIA root element unavailable");

        List<object> nodes = new List<object>();
        bool truncated = false;
        WalkAx(root, 0, depthLimit, nodeLimit, nodes, ref truncated);
        return new {
            ok = true,
            hwnd = targetHwnd.ToInt64(),
            truncated = truncated,
            nodes = nodes
        };
    }

    static void WalkAx(AutomationElement el, int depth, int maxDepth, int maxNodes, List<object> nodes, ref bool truncated) {
        if (nodes.Count >= maxNodes) {
            truncated = true;
            return;
        }
        try {
            nodes.Add(SerializeElement(el, depth));
        } catch (ElementNotAvailableException) {
            return;
        }
        if (depth >= maxDepth) return;

        AutomationElement child = null;
        try { child = TreeWalker.ControlViewWalker.GetFirstChild(el); } catch { return; }
        while (child != null) {
            if (nodes.Count >= maxNodes) {
                truncated = true;
                return;
            }
            WalkAx(child, depth + 1, maxDepth, maxNodes, nodes, ref truncated);
            try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); } catch { break; }
        }
    }

    public static object FindElements(long hwndVal, string windowTitle, string roleFilter, string titleFilter, string valueFilter, int limit) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, true);
        int cap = (limit > 0 && limit <= 5000) ? limit : 100;
        string normRole = string.IsNullOrEmpty(roleFilter) ? "" : NormalizeRole(roleFilter);

        AutomationElement root;
        try {
            root = AutomationElement.FromHandle(targetHwnd);
        } catch (Exception ex) {
            throw new HrnDesktopException("AxFailed", "AutomationElement.FromHandle failed: " + ex.Message);
        }
        if (root == null) throw new HrnDesktopException("AxFailed", "UIA root element unavailable");

        List<object> matches = new List<object>();
        int visited = 0;
        WalkFind(root, 0, normRole, titleFilter ?? "", valueFilter ?? "", cap, matches, ref visited);
        return new {
            ok = true,
            hwnd = targetHwnd.ToInt64(),
            matches = matches
        };
    }

    static void WalkFind(AutomationElement el, int depth, string normRole, string titleFilter, string valueFilter, int cap, List<object> matches, ref int visited) {
        if (matches.Count >= cap || visited >= 12000 || depth > 32) return;
        visited++;
        try {
            bool ok = true;
            if (normRole.Length > 0) {
                if (NormalizeRole(GetRoleName(el)) != normRole) ok = false;
            }
            if (ok && titleFilter.Length > 0) {
                string name = el.Current.Name ?? "";
                if (name.IndexOf(titleFilter, StringComparison.OrdinalIgnoreCase) < 0) ok = false;
            }
            if (ok && valueFilter.Length > 0) {
                string val = ReadElementValue(el, 500);
                if (val.IndexOf(valueFilter, StringComparison.OrdinalIgnoreCase) < 0) ok = false;
            }
            if (ok) {
                matches.Add(SerializeElement(el, depth));
            }
        } catch (ElementNotAvailableException) {
            return;
        }
        if (matches.Count >= cap) return;

        AutomationElement child = null;
        try { child = TreeWalker.ControlViewWalker.GetFirstChild(el); } catch { return; }
        while (child != null && matches.Count < cap && visited < 12000) {
            WalkFind(child, depth + 1, normRole, titleFilter, valueFilter, cap, matches, ref visited);
            try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); } catch { break; }
        }
    }

    static bool RuntimeIdsEqual(int[] a, int[] b) {
        if (a == null || b == null || a.Length != b.Length) return false;
        for (int i = 0; i < a.Length; i++) {
            if (a[i] != b[i]) return false;
        }
        return true;
    }

    static AutomationElement FindByRuntimeId(long hwndVal, int[] runtimeId) {
        if (runtimeId == null || runtimeId.Length == 0) {
            throw new HrnDesktopException("StaleRef", "missing runtime_id for element ref");
        }
        IntPtr h = new IntPtr(hwndVal);
        if (hwndVal == 0 || !IsWindow(h)) {
            throw new HrnDesktopException("StaleRef", "owning window is closed");
        }
        AutomationElement root;
        try {
            root = AutomationElement.FromHandle(h);
        } catch (Exception ex) {
            throw new HrnDesktopException("StaleRef", "owning window UIA unavailable: " + ex.Message);
        }
        if (root == null) throw new HrnDesktopException("StaleRef", "owning window UIA unavailable");

        try {
            if (RuntimeIdsEqual(root.GetRuntimeId(), runtimeId)) return root;
        } catch { }

        try {
            PropertyCondition cond = new PropertyCondition(AutomationElement.RuntimeIdProperty, runtimeId);
            AutomationElement fast = root.FindFirst(TreeScope.Subtree, cond);
            if (fast != null) return fast;
        } catch { }

        int visited = 0;
        AutomationElement found = WalkFindRuntimeId(root, runtimeId, 0, ref visited);
        if (found == null) {
            throw new HrnDesktopException("StaleRef", "element no longer exists in window UIA tree");
        }
        return found;
    }

    static AutomationElement WalkFindRuntimeId(AutomationElement el, int[] runtimeId, int depth, ref int visited) {
        if (visited >= 8000 || depth > 32) return null;
        visited++;
        try {
            if (RuntimeIdsEqual(el.GetRuntimeId(), runtimeId)) return el;
        } catch { return null; }

        AutomationElement child = null;
        try { child = TreeWalker.ControlViewWalker.GetFirstChild(el); } catch { return null; }
        while (child != null && visited < 8000) {
            AutomationElement hit = WalkFindRuntimeId(child, runtimeId, depth + 1, ref visited);
            if (hit != null) return hit;
            try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); } catch { break; }
        }
        return null;
    }

    public static object ElRead(long hwndVal, int[] runtimeId, string field) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        string f = (field ?? "value").Trim().ToLowerInvariant();
        if (f == "value") {
            return new { ok = true, field = "value", value = ReadElementValue(el, 0) };
        }
        if (f == "bounds") {
            Rect br = el.Current.BoundingRectangle;
            int x = 0, y = 0, w = 0, h = 0;
            if (!br.IsEmpty && !double.IsInfinity(br.X) && !double.IsNaN(br.X)) {
                x = (int)Math.Round(br.X);
                y = (int)Math.Round(br.Y);
                w = Math.Max(0, (int)Math.Round(br.Width));
                h = Math.Max(0, (int)Math.Round(br.Height));
            }
            return new { ok = true, field = "bounds", x = x, y = y, w = w, h = h };
        }
        if (f == "actions") {
            List<string> actions = new List<string>();
            AutomationPattern[] pats = el.GetSupportedPatterns();
            for (int i = 0; i < pats.Length; i++) {
                AutomationPattern p = pats[i];
                if (p == InvokePattern.Pattern) actions.Add("invoke");
                else if (p == TogglePattern.Pattern) actions.Add("toggle");
                else if (p == SelectionItemPattern.Pattern) actions.Add("select");
                else if (p == ExpandCollapsePattern.Pattern) actions.Add("expand_collapse");
                else if (p == ValuePattern.Pattern) actions.Add("set_value");
                else if (p == TextPattern.Pattern) actions.Add("select_text");
                else if (p == ScrollPattern.Pattern) actions.Add("scroll");
                else if (p == RangeValuePattern.Pattern) actions.Add("range_value");
            }
            if (el.Current.IsKeyboardFocusable) actions.Add("focus");
            return new { ok = true, field = "actions", actions = actions };
        }
        if (f == "parent") {
            AutomationElement parent = TreeWalker.ControlViewWalker.GetParent(el);
            object node = parent != null ? SerializeElement(parent, 0) : null;
            return new { ok = true, field = "parent", parent = node };
        }
        if (f == "children") {
            List<object> kids = new List<object>();
            AutomationElement child = TreeWalker.ControlViewWalker.GetFirstChild(el);
            while (child != null && kids.Count < 200) {
                try { kids.Add(SerializeElement(child, 1)); } catch { }
                try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); } catch { break; }
            }
            return new { ok = true, field = "children", children = kids };
        }
        throw new HrnDesktopException("AxFailed", "unknown el_read field: " + field);
    }

    static string NormalizeNewlines(string s) {
        if (s == null) return "";
        return s.Replace("\r\n", "\n").Replace("\r", "\n");
    }

    static bool ValueMatches(string actual, string expected) {
        string a = NormalizeNewlines(actual);
        string e = NormalizeNewlines(expected);
        if (a == e) return true;
        if (!e.EndsWith("\n") && a == e + "\n") return true;
        return false;
    }

    public static object SetValue(long hwndVal, int[] runtimeId, string value) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        string targetVal = value ?? "";
        object patObj;
        if (el.TryGetCurrentPattern(ValuePattern.Pattern, out patObj)) {
            ValuePattern vp = (ValuePattern)patObj;
            if (vp.Current.IsReadOnly) {
                throw new HrnDesktopException("AxUnconfirmed", "element ValuePattern is read-only");
            }
            vp.SetValue(targetVal);
            Thread.Sleep(30);
            string readback = ReadElementValue(el, 0);
            if (!ValueMatches(readback, targetVal)) {
                throw new HrnDesktopException("AxUnconfirmed", "set_value read-back verification failed");
            }
            return new { ok = true, verified = true, value = readback };
        }
        string role = NormalizeRole(GetRoleName(el));
        int nativeHwnd = 0;
        try { nativeHwnd = el.Current.NativeWindowHandle; } catch { }
        if ((role == "edit" || role == "document") && nativeHwnd != 0) {
            SendMessage(new IntPtr(nativeHwnd), WM_SETTEXT, IntPtr.Zero, targetVal);
            Thread.Sleep(30);
            string readback = ReadElementValue(el, 0);
            if (ValueMatches(readback, targetVal)) {
                return new { ok = true, verified = true, value = readback };
            }
        }
        throw new HrnDesktopException("AxUnconfirmed", "element does not support ValuePattern or value remained unchanged");
    }

    public static object SelectText(long hwndVal, int[] runtimeId, bool hasStart, int start, bool hasEnd, int end) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        object patObj;
        if (el.TryGetCurrentPattern(TextPattern.Pattern, out patObj)) {
            TextPattern tp = (TextPattern)patObj;
            TextPatternRange range = tp.DocumentRange.Clone();
            if (hasStart || hasEnd) {
                int s = hasStart ? Math.Max(0, start) : 0;
                range.MoveEndpointByUnit(TextPatternRangeEndpoint.Start, TextUnit.Character, s);
                if (hasEnd) {
                    int len = Math.Max(0, end - s);
                    range.MoveEndpointByRange(TextPatternRangeEndpoint.End, range, TextPatternRangeEndpoint.Start);
                    range.MoveEndpointByUnit(TextPatternRangeEndpoint.End, TextUnit.Character, len);
                }
            }
            range.Select();
            string sel = range.GetText(500);
            return new { ok = true, selected = sel ?? "" };
        }
        int nativeHwnd = 0;
        try { nativeHwnd = el.Current.NativeWindowHandle; } catch { }
        if (nativeHwnd != 0) {
            int s = hasStart ? start : 0;
            int e = hasEnd ? end : -1;
            SendMessagePtr(new IntPtr(nativeHwnd), EM_SETSEL, new IntPtr(s), new IntPtr(e));
            return new { ok = true, start = s, end = e };
        }
        if (el.TryGetCurrentPattern(ValuePattern.Pattern, out patObj) && !hasStart && !hasEnd) {
            el.SetFocus();
            Thread.Sleep(20);
            Hotkey(new int[] { 0x11, 0x41 });
            return new { ok = true, selected = ReadElementValue(el, 500) };
        }
        throw new HrnDesktopException("AxFailed", "element does not support text selection");
    }

    public static object InvokeElement(long hwndVal, int[] runtimeId) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        object pat;
        if (el.TryGetCurrentPattern(InvokePattern.Pattern, out pat)) {
            ((InvokePattern)pat).Invoke();
            return new { ok = true, pattern = "invoke" };
        }
        if (el.TryGetCurrentPattern(TogglePattern.Pattern, out pat)) {
            TogglePattern tp = (TogglePattern)pat;
            tp.Toggle();
            return new { ok = true, pattern = "toggle", state = tp.Current.ToggleState.ToString() };
        }
        if (el.TryGetCurrentPattern(SelectionItemPattern.Pattern, out pat)) {
            ((SelectionItemPattern)pat).Select();
            return new { ok = true, pattern = "select" };
        }
        if (el.TryGetCurrentPattern(ExpandCollapsePattern.Pattern, out pat)) {
            ExpandCollapsePattern ep = (ExpandCollapsePattern)pat;
            if (ep.Current.ExpandCollapseState == ExpandCollapseState.Collapsed) ep.Expand();
            else ep.Collapse();
            return new { ok = true, pattern = "expand_collapse", state = ep.Current.ExpandCollapseState.ToString() };
        }
        throw new HrnDesktopException("AxFailed", "element does not support Invoke, Toggle, SelectionItem, or ExpandCollapse");
    }

    public static object FocusElement(long hwndVal, int[] runtimeId) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        el.SetFocus();
        return new { ok = true };
    }

    public static object TypeElement(long hwndVal, int[] runtimeId, string text) {
        AutomationElement el = FindByRuntimeId(hwndVal, runtimeId);
        el.SetFocus();
        Thread.Sleep(30);
        string s = text ?? "";
        TypeUnicode(s);
        return new { ok = true, chars = s.Length };
    }

    public static void Move(int x, int y) { SetCursorPos(x, y); }

    public static void Click(int x, int y, string kind) {
        SetCursorPos(x, y);
        Thread.Sleep(20);
        if (kind == "right") {
            mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, UIntPtr.Zero);
            mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, UIntPtr.Zero);
        } else if (kind == "double") {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, UIntPtr.Zero);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, UIntPtr.Zero);
            Thread.Sleep(40);
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, UIntPtr.Zero);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, UIntPtr.Zero);
        } else {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, UIntPtr.Zero);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, UIntPtr.Zero);
        }
    }

    public static void Scroll(int delta, bool hasPos, int x, int y) {
        if (hasPos) { SetCursorPos(x, y); Thread.Sleep(15); }
        mouse_event(MOUSEEVENTF_WHEEL, 0, 0, delta * 120, UIntPtr.Zero);
    }

    public static void TypeUnicode(string text) {
        foreach (char ch in text) {
            if (ch == '\r') continue;
            if (ch == '\n') {
                keybd_event(0x0D, 0, 0, UIntPtr.Zero);
                keybd_event(0x0D, 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
                continue;
            }
            INPUT[] inputs = new INPUT[2];
            inputs[0].type = 1;
            inputs[0].u.ki.wScan = (ushort)ch;
            inputs[0].u.ki.dwFlags = KEYEVENTF_UNICODE;
            inputs[1].type = 1;
            inputs[1].u.ki.wScan = (ushort)ch;
            inputs[1].u.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
            SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT)));
        }
    }

    public static void Hotkey(int[] vks) {
        foreach (int vk in vks) keybd_event((byte)vk, 0, 0, UIntPtr.Zero);
        Thread.Sleep(25);
        for (int i = vks.Length - 1; i >= 0; i--) keybd_event((byte)vks[i], 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
    }

    static IntPtr MakeLParam(int low, int high) {
        int packed = ((high & 0xFFFF) << 16) | (low & 0xFFFF);
        return new IntPtr(packed);
    }

    static IntPtr FindFocusChild(IntPtr topHwnd) {
        uint pid;
        uint tid = GetWindowThreadProcessId(topHwnd, out pid);
        if (tid != 0) {
            GUITHREADINFO info = new GUITHREADINFO();
            info.cbSize = Marshal.SizeOf(typeof(GUITHREADINFO));
            if (GetGUIThreadInfo(tid, ref info) && info.hwndFocus != IntPtr.Zero && IsWindow(info.hwndFocus)) {
                return info.hwndFocus;
            }
        }
        return topHwnd;
    }

    static bool WaitForInputDrain(IntPtr targetHwnd, uint timeoutMs) {
        IntPtr dummy;
        IntPtr res = SendMessageTimeout(targetHwnd, WM_NULL, IntPtr.Zero, IntPtr.Zero, SMTO_ABORTIFHUNG, timeoutMs, out dummy);
        return res != IntPtr.Zero;
    }

    static object RunTakeover(IntPtr targetHwnd, Action performInput, string expectedText) {
        if (IsScreenLocked()) {
            throw new HrnDesktopException("ScreenLocked", "desktop is locked or on secure desktop");
        }
        IntPtr prevFg = GetForegroundWindow();
        uint curTid = GetCurrentThreadId();
        uint dummyPid;
        uint prevTid = prevFg != IntPtr.Zero ? GetWindowThreadProcessId(prevFg, out dummyPid) : 0;
        uint targetTid = GetWindowThreadProcessId(targetHwnd, out dummyPid);

        bool attachedPrev = false;
        bool attachedTarget = false;
        try {
            if (prevTid != 0 && prevTid != curTid) attachedPrev = AttachThreadInput(curTid, prevTid, true);
            if (targetTid != 0 && targetTid != curTid) attachedTarget = AttachThreadInput(curTid, targetTid, true);

            if (IsIconic(targetHwnd)) ShowWindow(targetHwnd, SW_RESTORE);
            SetForegroundWindow(targetHwnd);
            int waited = 0;
            while (GetForegroundWindow() != targetHwnd && waited < 300) {
                Thread.Sleep(20);
                waited += 20;
            }

            performInput();
            bool drained = WaitForInputDrain(targetHwnd, 5000);
            string unconfirmed = drained ? "" : (string.IsNullOrEmpty(expectedText) ? "input drain timed out" : expectedText);
            return new {
                ok = true,
                takeover = true,
                unconfirmed = unconfirmed
            };
        } finally {
            if (prevFg != IntPtr.Zero && prevFg != targetHwnd && IsWindow(prevFg)) {
                SetForegroundWindow(prevFg);
            }
            if (attachedTarget) AttachThreadInput(curTid, targetTid, false);
            if (attachedPrev) AttachThreadInput(curTid, prevTid, false);
        }
    }

    public static object BgClick(long hwndVal, string windowTitle, int screenX, int screenY, string kind, bool takeover) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, false);
        if (targetHwnd == IntPtr.Zero) throw new HrnDesktopException("WindowNotFound", "target window required for bg_click");
        if (IsHungAppWindow(targetHwnd)) throw new HrnDesktopException("InputBusy", "target window is unresponsive");
        if (takeover) {
            return RunTakeover(targetHwnd, delegate() { Click(screenX, screenY, kind); }, "");
        }
        if (IsIconic(targetHwnd)) {
            throw new HrnDesktopException("BackgroundUnavailable", "target window is minimized");
        }
        POINT pt;
        pt.X = screenX;
        pt.Y = screenY;
        ScreenToClient(targetHwnd, ref pt);
        IntPtr dest = targetHwnd;
        IntPtr child = ChildWindowFromPointEx(targetHwnd, pt, CWP_SKIPINVISIBLE);
        if (child != IntPtr.Zero && child != targetHwnd) {
            POINT cpt;
            cpt.X = screenX;
            cpt.Y = screenY;
            ScreenToClient(child, ref cpt);
            dest = child;
            pt = cpt;
        }
        IntPtr lParam = MakeLParam(pt.X, pt.Y);
        bool sent;
        if (kind == "right") {
            sent = PostMessage(dest, WM_RBUTTONDOWN, new IntPtr(MK_RBUTTON), lParam)
                && PostMessage(dest, WM_RBUTTONUP, IntPtr.Zero, lParam);
        } else if (kind == "double") {
            sent = PostMessage(dest, WM_LBUTTONDOWN, new IntPtr(MK_LBUTTON), lParam)
                && PostMessage(dest, WM_LBUTTONUP, IntPtr.Zero, lParam)
                && PostMessage(dest, WM_LBUTTONDBLCLK, new IntPtr(MK_LBUTTON), lParam)
                && PostMessage(dest, WM_LBUTTONUP, IntPtr.Zero, lParam);
        } else {
            sent = PostMessage(dest, WM_LBUTTONDOWN, new IntPtr(MK_LBUTTON), lParam)
                && PostMessage(dest, WM_LBUTTONUP, IntPtr.Zero, lParam);
        }
        if (!sent) {
            throw new HrnDesktopException("BackgroundUnavailable", "PostMessage click rejected by target window");
        }
        return new { ok = true, takeover = false, background = true };
    }

    public static object BgType(long hwndVal, string windowTitle, string text, bool takeover) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, false);
        if (targetHwnd == IntPtr.Zero) throw new HrnDesktopException("WindowNotFound", "target window required for bg_type");
        if (IsHungAppWindow(targetHwnd)) throw new HrnDesktopException("InputBusy", "target window is unresponsive");
        string s = text ?? "";
        if (takeover) {
            return RunTakeover(targetHwnd, delegate() { TypeUnicode(s); }, s);
        }
        if (IsIconic(targetHwnd)) {
            throw new HrnDesktopException("BackgroundUnavailable", "target window is minimized");
        }
        IntPtr dest = FindFocusChild(targetHwnd);
        foreach (char ch in s) {
            if (ch == '\r') continue;
            bool ok = (ch == '\n')
                ? PostMessage(dest, WM_KEYDOWN, new IntPtr(0x0D), IntPtr.Zero) && PostMessage(dest, WM_KEYUP, new IntPtr(0x0D), IntPtr.Zero)
                : PostMessage(dest, WM_CHAR, new IntPtr(ch), IntPtr.Zero);
            if (!ok) {
                throw new HrnDesktopException("BackgroundUnavailable", "PostMessage typing rejected by target window");
            }
        }
        return new { ok = true, takeover = false, background = true, chars = s.Length };
    }

    public static object BgHotkey(long hwndVal, string windowTitle, int[] vks, bool takeover) {
        IntPtr targetHwnd = ResolveWindow(hwndVal, windowTitle, false);
        if (targetHwnd == IntPtr.Zero) throw new HrnDesktopException("WindowNotFound", "target window required for bg_hotkey");
        if (IsHungAppWindow(targetHwnd)) throw new HrnDesktopException("InputBusy", "target window is unresponsive");
        if (takeover) {
            return RunTakeover(targetHwnd, delegate() { Hotkey(vks); }, "");
        }
        if (IsIconic(targetHwnd)) {
            throw new HrnDesktopException("BackgroundUnavailable", "target window is minimized");
        }
        if (vks.Length > 1 && GetForegroundWindow() != targetHwnd) {
            throw new HrnDesktopException("BackgroundUnavailable", "modifier hotkey chord requires foreground focus");
        }
        IntPtr dest = FindFocusChild(targetHwnd);
        foreach (int vk in vks) {
            if (!PostMessage(dest, WM_KEYDOWN, new IntPtr(vk), IntPtr.Zero)) {
                throw new HrnDesktopException("BackgroundUnavailable", "PostMessage keydown rejected");
            }
        }
        for (int i = vks.Length - 1; i >= 0; i--) {
            if (!PostMessage(dest, WM_KEYUP, new IntPtr(vks[i]), IntPtr.Zero)) {
                throw new HrnDesktopException("BackgroundUnavailable", "PostMessage keyup rejected");
            }
        }
        return new { ok = true, takeover = false, background = true };
    }
}
"@

[HrnWinDesktop]::Init()

function To-IntArray($raw) {
    if ($null -eq $raw) { return [int[]]@() }
    return [int[]]@($raw | ForEach-Object { [int]$_ })
}

function To-StrArray($raw) {
    if ($null -eq $raw) { return [string[]]@() }
    return [string[]]@($raw | ForEach-Object { [string]$_ })
}

while ($true) {
    $line = [Console]::In.ReadLine()
    if ($null -eq $line) { break }
    if ([string]::IsNullOrWhiteSpace($line)) { continue }
    try {
        $req = $line | ConvertFrom-Json
        $op = [string]$req.op
        switch ($op) {
            'screenshot' {
                $res = [HrnWinDesktop]::Capture(
                    [bool]$req.all_monitors,
                    [string]$req.window,
                    [long]$req.hwnd,
                    [string]$req.display,
                    [int]$req.max_width,
                    [int]$req.max_height
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'window_capture' {
                $res = [HrnWinDesktop]::WindowCapture(
                    [long]$req.hwnd,
                    [string]$req.window,
                    [int]$req.max_width,
                    [int]$req.max_height
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'window_rect' {
                $res = [HrnWinDesktop]::WindowRect([long]$req.hwnd)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'zoom' {
                $res = [HrnWinDesktop]::Zoom(
                    [int]$req.x,
                    [int]$req.y,
                    [int]$req.w,
                    [int]$req.h,
                    [long]$req.hwnd,
                    [string]$req.window,
                    [int]$req.max_width,
                    [int]$req.max_height
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'list_windows' {
                $res = [HrnWinDesktop]::ListWindows()
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'displays' {
                $res = [HrnWinDesktop]::Displays()
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'capabilities' {
                $res = [HrnWinDesktop]::Capabilities()
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'apps_list' {
                $res = [HrnWinDesktop]::AppsList()
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 6))
            }
            'open_app' {
                $res = [HrnWinDesktop]::OpenApp([string]$req.target, (To-StrArray $req.args))
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'clipboard_read' {
                $res = [HrnWinDesktop]::ClipboardRead()
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'clipboard_write' {
                $res = [HrnWinDesktop]::ClipboardWrite([string]$req.text)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'ax' {
                $md = if ($null -ne $req.max_depth) { [int]$req.max_depth } else { 24 }
                $mn = if ($null -ne $req.max_nodes) { [int]$req.max_nodes } else { 800 }
                $res = [HrnWinDesktop]::AxTree([long]$req.hwnd, [string]$req.window, $md, $mn)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 8))
            }
            'find' {
                $lim = if ($null -ne $req.limit) { [int]$req.limit } else { 100 }
                $res = [HrnWinDesktop]::FindElements(
                    [long]$req.hwnd,
                    [string]$req.window,
                    [string]$req.role,
                    [string]$req.title,
                    [string]$req.value,
                    $lim
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 8))
            }
            'el_read' {
                $res = [HrnWinDesktop]::ElRead([long]$req.hwnd, (To-IntArray $req.runtime_id), [string]$req.field)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 8))
            }
            'set_value' {
                $res = [HrnWinDesktop]::SetValue([long]$req.hwnd, (To-IntArray $req.runtime_id), [string]$req.value)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'select_text' {
                $hs = ($null -ne $req.start)
                $he = ($null -ne $req.end)
                $s = if ($hs) { [int]$req.start } else { 0 }
                $e = if ($he) { [int]$req.end } else { 0 }
                $res = [HrnWinDesktop]::SelectText([long]$req.hwnd, (To-IntArray $req.runtime_id), $hs, $s, $he, $e)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'invoke' {
                $res = [HrnWinDesktop]::InvokeElement([long]$req.hwnd, (To-IntArray $req.runtime_id))
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'focus' {
                $res = [HrnWinDesktop]::FocusElement([long]$req.hwnd, (To-IntArray $req.runtime_id))
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'el_type' {
                $res = [HrnWinDesktop]::TypeElement([long]$req.hwnd, (To-IntArray $req.runtime_id), [string]$req.text)
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'move' {
                [HrnWinDesktop]::Move([int]$req.x, [int]$req.y)
                [Console]::Out.WriteLine('{"ok":true}')
            }
            'click' {
                [HrnWinDesktop]::Click([int]$req.x, [int]$req.y, [string]$req.kind)
                [Console]::Out.WriteLine('{"ok":true}')
            }
            'scroll' {
                [HrnWinDesktop]::Scroll([int]$req.delta, [bool]$req.has_pos, [int]$req.x, [int]$req.y)
                [Console]::Out.WriteLine('{"ok":true}')
            }
            'type_text' {
                [HrnWinDesktop]::TypeUnicode([string]$req.text)
                [Console]::Out.WriteLine('{"ok":true}')
            }
            'hotkey' {
                [HrnWinDesktop]::Hotkey((To-IntArray $req.vks))
                [Console]::Out.WriteLine('{"ok":true}')
            }
            'bg_click' {
                $res = [HrnWinDesktop]::BgClick(
                    [long]$req.hwnd,
                    [string]$req.window,
                    [int]$req.x,
                    [int]$req.y,
                    [string]$req.kind,
                    [bool]$req.takeover
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'bg_type' {
                $res = [HrnWinDesktop]::BgType(
                    [long]$req.hwnd,
                    [string]$req.window,
                    [string]$req.text,
                    [bool]$req.takeover
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            'bg_hotkey' {
                $res = [HrnWinDesktop]::BgHotkey(
                    [long]$req.hwnd,
                    [string]$req.window,
                    (To-IntArray $req.vks),
                    [bool]$req.takeover
                )
                [Console]::Out.WriteLine(($res | ConvertTo-Json -Compress -Depth 4))
            }
            default {
                $err = @{ ok = $false; code = 'AxFailed'; error = "unknown op: $op" } | ConvertTo-Json -Compress
                [Console]::Out.WriteLine($err)
            }
        }
    } catch {
        $ex = $_.Exception
        while ($null -ne $ex.InnerException -and -not ($ex -is [HrnDesktopException])) {
            $ex = $ex.InnerException
        }
        $code = if ($ex -is [HrnDesktopException]) { $ex.Code } else { 'AxFailed' }
        $err = @{ ok = $false; code = $code; error = $ex.Message } | ConvertTo-Json -Compress
        [Console]::Out.WriteLine($err)
    }
    [Console]::Out.Flush()
}
