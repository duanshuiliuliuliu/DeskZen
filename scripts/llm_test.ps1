$cfgPath = Join-Path $env:APPDATA "com.deskzen.app\llm.json"
$cfg = Get-Content -LiteralPath $cfgPath | ConvertFrom-Json

$body = @{
    model = $cfg.model
    messages = @(
        @{ role = "system"; content = "You are a pixel puppy. Reply briefly with a bit of bark flavor." },
        @{ role = "user"; content = "Say hi and introduce yourself in one sentence" }
    )
    temperature = 0.8
    max_tokens = 64
    stream = $false
} | ConvertTo-Json -Depth 6

$headers = @{ Authorization = "Bearer $($cfg.api_key)" }
$resp = Invoke-RestMethod -Method Post -Uri "$($cfg.base_url)/chat/completions" -Headers $headers -ContentType "application/json" -Body $body
Write-Output ("model: {0}" -f $resp.model)
Write-Output ("reply: {0}" -f $resp.choices[0].message.content)
