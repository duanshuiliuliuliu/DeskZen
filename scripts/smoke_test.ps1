param(
    [string]$Exe = "C:\Myfiles\repo\nbs\DeskZen\src-tauri\target\debug\deskzen.exe",
    [switch]$SkipDrag
)

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class Win32Smoke {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
    [DllImport("user32.dll")] public static extern uint GetWindowLong(IntPtr hWnd, int idx);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@

function Find-Windows([string]$titlePart, [int]$targetPid) {
    $list = [System.Collections.ArrayList]::new()
    $cb = [Win32Smoke+EnumProc]{
        param($h, $l)
        $sb = New-Object System.Text.StringBuilder 256
        $p = 0
        [void][Win32Smoke]::GetWindowText($h, $sb, 256)
        [void][Win32Smoke]::GetWindowThreadProcessId($h, [ref]$p)
        if ([Win32Smoke]::IsWindowVisible($h) -and $sb.ToString().Contains($titlePart) -and $p -eq $targetPid) {
            [void]$list.Add(@{ Handle = $h; Title = $sb.ToString() })
        }
        return $true
    }
    [void][Win32Smoke]::EnumWindows($cb, [IntPtr]::Zero)
    return $list
}

function Get-Rect([IntPtr]$h) {
    $r = New-Object Win32Smoke+RECT
    [void][Win32Smoke]::GetWindowRect($h, [ref]$r)
    return [pscustomobject]@{
        Left = [int]$r.Left
        Top = [int]$r.Top
        Width = [int]($r.Right - $r.Left)
        Height = [int]($r.Bottom - $r.Top)
    }
}

function Find-ChatWindow {
    $all = @(Find-Windows "DeskZen" $script:AppPid)
    foreach ($w in $all) {
        $r = Get-Rect $w.Handle
        if ($r.Height -gt 400) {
            return $w.Handle
        }
    }
    return [IntPtr]::Zero
}

function List-AppWindows {
    $cb = [Win32Smoke+EnumProc]{
        param($h, $l)
        $sb = New-Object System.Text.StringBuilder 256
        $p = 0
        [void][Win32Smoke]::GetWindowText($h, $sb, 256)
        [void][Win32Smoke]::GetWindowThreadProcessId($h, [ref]$p)
        if ($p -eq $script:AppPid) {
            $r = New-Object Win32Smoke+RECT
            [void][Win32Smoke]::GetWindowRect($h, [ref]$r)
            Write-Output ("  hwnd={0} visible={1} title='{2}' rect={3},{4} {5}x{6}" -f $h, [Win32Smoke]::IsWindowVisible($h), $sb.ToString(), $r.Left, $r.Top, ($r.Right - $r.Left), ($r.Bottom - $r.Top))
        }
        return $true
    }
    [void][Win32Smoke]::EnumWindows($cb, [IntPtr]::Zero)
}

function Send-Click([int]$x, [int]$y) {
    [void][Win32Smoke]::SetCursorPos($x, $y)
    Start-Sleep -Milliseconds 200
    [Win32Smoke]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero) # LEFTDOWN
    Start-Sleep -Milliseconds 100
    [Win32Smoke]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero) # LEFTUP
}

function Send-Drag([int]$x, [int]$y) {
    [void][Win32Smoke]::SetCursorPos($x, $y)
    Start-Sleep -Milliseconds 200
    [Win32Smoke]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    for ($i = 1; $i -le 10; $i++) {
        [void][Win32Smoke]::SetCursorPos($x + [int]($i * 8), $y + [int]($i * 6))
        Start-Sleep -Milliseconds 60
    }
    [Win32Smoke]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
}

function Invoke-UiaClose([IntPtr]$hwnd) {
    try {
        $ae = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
        $btnCond = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Button)
        $closeBtn = $null
        foreach ($name in @("关闭", "Close")) {
            $nameCond = New-Object System.Windows.Automation.PropertyCondition(
                [System.Windows.Automation.AutomationElement]::NameProperty, $name)
            $cond = New-Object System.Windows.Automation.AndCondition($btnCond, $nameCond)
            $closeBtn = $ae.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
            if ($closeBtn) { break }
        }
        if (-not $closeBtn) { return "no-close-button" }
        $pattern = $closeBtn.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $pattern.Invoke()
        return "invoked"
    } catch {
        return "uia-error: $($_.Exception.Message)"
    }
}

$existing = Get-Process deskzen -ErrorAction SilentlyContinue | Select-Object -First 1
if ($existing) {
    $p = $existing
    $attached = $true
    Write-Output "attaching to running deskzen pid=$($p.Id)"
} else {
    $p = Start-Process -FilePath $Exe -PassThru
    $attached = $false
    Start-Sleep -Seconds 10
}
$script:AppPid = $p.Id

$personas = @(Find-Windows "DeskZen" $p.Id)
if ($personas.Count -eq 0) {
    Write-Output "FAIL: persona window not found"
    if (-not $attached) { Stop-Process -Id $p.Id -Force }
    exit 1
}
$persona = $personas[0].Handle
$r = Get-Rect $persona
Write-Output ("persona: title='{0}' rect={1},{2} {3}x{4}" -f $personas[0].Title, $r.Left, $r.Top, $r.Width, $r.Height)

# 1) drag first (no chat window open yet)
if (-not $SkipDrag) {
    $r2 = Get-Rect $persona
    Send-Drag ($r2.Left + 120) ($r2.Top + 170)
    Start-Sleep -Seconds 1
    $r3 = Get-Rect $persona
    $moved = ($r3.Left -ne $r2.Left) -or ($r3.Top -ne $r2.Top)
    Write-Output ("drag: before=({0},{1}) after=({2},{3}) moved={4}" -f $r2.Left, $r2.Top, $r3.Left, $r3.Top, $moved)
} else {
    Write-Output "drag: skipped"
}

# 2) click -> chat opens
$r = Get-Rect $persona
Send-Click ($r.Left + 120) ($r.Top + 170)
Start-Sleep -Seconds 3
$chat = Find-ChatWindow
if ($chat -eq [IntPtr]::Zero) {
    # 首次点击可能落在页面初始化窗口期，重试一次
    Start-Sleep -Seconds 2
    $r = Get-Rect $persona
    Send-Click ($r.Left + 120) ($r.Top + 170)
    Start-Sleep -Seconds 3
    $chat = Find-ChatWindow
}
if ($chat -eq [IntPtr]::Zero) {
    Write-Output "FAIL: chat did not open after click"
    Write-Output "app windows:"
    List-AppWindows
    if (-not $attached) { Stop-Process -Id $p.Id -Force }
    exit 1
}
$cr = Get-Rect $chat
$style = [Win32Smoke]::GetWindowLong($chat, -16)
Write-Output ("chat: rect={0},{1} {2}x{3} caption={4} sysmenu={5}" -f $cr.Left, $cr.Top, $cr.Width, $cr.Height, (($style -band 0xC00000) -eq 0xC00000), (($style -band 0x00080000) -ne 0))

# 3) close via UIA close button (real user path)
$uiaResult = Invoke-UiaClose $chat
Start-Sleep -Seconds 2
$chat2 = Find-ChatWindow
$closeState = if ($chat2 -eq [IntPtr]::Zero) { "CLOSED" } else { "STILL OPEN" }
Write-Output ("uia close: {0} -> {1}" -f $uiaResult, $closeState)

# 4) fallback: WM_CLOSE
if ($chat2 -ne [IntPtr]::Zero) {
    [void][Win32Smoke]::PostMessage($chat, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
    Start-Sleep -Seconds 2
    $chat3 = Find-ChatWindow
    $closeState2 = if ($chat3 -eq [IntPtr]::Zero) { "CLOSED" } else { "STILL OPEN" }
    Write-Output ("wm_close -> {0}" -f $closeState2)
}

# 5) 对话窗跟随角色移动
$r = Get-Rect $persona
Send-Click ($r.Left + 120) ($r.Top + 170)
Start-Sleep -Seconds 2
$chat4 = Find-ChatWindow
if ($chat4 -ne [IntPtr]::Zero) {
    $cb = Get-Rect $chat4
    $r = Get-Rect $persona
    Send-Drag ($r.Left + 120) ($r.Top + 170)
    Start-Sleep -Seconds 1
    $ca = Get-Rect $chat4
    $cMoved = ($ca.Left -ne $cb.Left) -or ($ca.Top -ne $cb.Top)
    Write-Output ("follow: chat before=({0},{1}) after=({2},{3}) moved={4}" -f $cb.Left, $cb.Top, $ca.Left, $ca.Top, $cMoved)
} else {
    Write-Output "follow: chat did not reopen"
}

if (-not $attached -and -not $p.HasExited) {
    Stop-Process -Id $p.Id -Force
}
Write-Output "smoke test done"
