param(
    [string]$Exe = "C:\Myfiles\repo\nbs\DeskZen\src-tauri\target\release\deskzen.exe",
    [int]$Port = 9226
)

$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
$p = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds 8

try {
    $targets = Invoke-RestMethod "http://127.0.0.1:$Port/json" -TimeoutSec 5
    foreach ($t in $targets) {
        Write-Output ("target: type={0} title='{1}' url={2}" -f $t.type, $t.title, $t.url)
    }
} catch {
    Write-Output "devtools endpoint not reachable: $($_.Exception.Message)"
}

if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
