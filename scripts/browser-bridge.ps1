# Dedicated browser bridge. Stdout is a private child-process pipe consumed by Rust.
# Never run interactively: its sole successful output is the protected session payload.
param([int]$Port = 19227, [switch]$Attach)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
try {
    if ($Port -lt 1024 -or $Port -gt 65535) { throw 'invalid port' }
    function Test-DedicatedBrowser {
        $listeners = @(Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue)
        if ($listeners.Count -eq 0) { return $false }
        foreach ($listener in $listeners) {
            if ($listener.LocalAddress -notin @('127.0.0.1', '::1')) { throw 'non-loopback debugger refused' }
            $browser = Get-CimInstance Win32_Process -Filter "ProcessId = $($listener.OwningProcess)"
            if ($browser.Name -ne 'msedge.exe') { throw 'only dedicated Edge is allowed' }
            $command = [string]$browser.CommandLine
            if ($command -notmatch "--remote-debugging-port(?:=|\s+)$Port(?:\s|$)" -or
                $command -notmatch '--remote-debugging-address(?:=|\s+)(?:127\.0\.0\.1|localhost)(?:\s|$)' -or
                $command -notmatch '(?i)--user-data-dir(?:=|\s+)"?[^"\r\n]*[\\/](?:YikeBackupLogin|yike-browser)"?(?:\s|$)') {
                throw 'dedicated profile proof unavailable'
            }
        }
        return $true
    }
    if (-not (Test-DedicatedBrowser)) {
        if ($Attach) { throw 'dedicated debugger is not running' }
        $profile = Join-Path $env:LOCALAPPDATA 'Codex\YikeBackupLogin'
        $edge = Join-Path ${env:ProgramFiles(x86)} 'Microsoft\Edge\Application\msedge.exe'
        if (-not (Test-Path -LiteralPath $edge)) { $edge = Join-Path $env:ProgramFiles 'Microsoft\Edge\Application\msedge.exe' }
        if (-not (Test-Path -LiteralPath $edge)) { throw 'Edge unavailable' }
        $null = New-Item -ItemType Directory -Path $profile -Force
        $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
        $acl = Get-Acl -LiteralPath $profile
        $acl.SetAccessRuleProtection($true, $false)
        foreach ($rule in @($acl.Access)) { $acl.RemoveAccessRuleSpecific($rule) }
        $acl.SetOwner($identity)
        $rule = [Security.AccessControl.FileSystemAccessRule]::new($identity, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
        Set-Acl -LiteralPath $profile -AclObject $acl
        $arguments = @("--remote-debugging-port=$Port", '--remote-debugging-address=127.0.0.1', "--user-data-dir=`"$profile`"", '--enable-automation', '--no-first-run', '--new-window', 'https://photo.baidu.com/')
        $null = Start-Process -FilePath $edge -ArgumentList $arguments -PassThru
    }
    $script:sequence = 0
    function Open-Cdp([string]$Address) {
        $uri = [Uri]$Address
        if ($uri.Scheme -ne 'ws' -or $uri.Host -notin @('127.0.0.1', 'localhost', '[::1]', '::1') -or $uri.Port -ne $Port) { throw 'untrusted debugger URL' }
        $socket = [System.Net.WebSockets.ClientWebSocket]::new()
        $socket.Options.Proxy = $null
        $cancellation = [Threading.CancellationTokenSource]::new()
        $cancellation.CancelAfter(15000)
        try { [void]$socket.ConnectAsync($uri, $cancellation.Token).GetAwaiter().GetResult() }
        catch { $socket.Dispose(); throw }
        finally { $cancellation.Dispose() }
        return ,$socket
    }
    function Invoke-Cdp($Socket, [string]$Method, $Parameters) {
        $cancellation = [Threading.CancellationTokenSource]::new()
        $cancellation.CancelAfter(15000)
        try {
        $script:sequence++
        $requestId = $script:sequence
        $payload = @{ id = $requestId; method = $Method; params = $Parameters } | ConvertTo-Json -Depth 12 -Compress
        $bytes = [Text.Encoding]::UTF8.GetBytes($payload)
        [void]$Socket.SendAsync([ArraySegment[byte]]::new($bytes), [Net.WebSockets.WebSocketMessageType]::Text, $true, $cancellation.Token).GetAwaiter().GetResult()
        while ($true) {
            $memory = [IO.MemoryStream]::new()
            try {
                do {
                    $buffer = [byte[]]::new(16384)
                    $received = $Socket.ReceiveAsync([ArraySegment[byte]]::new($buffer), $cancellation.Token).GetAwaiter().GetResult()
                    if ($received.MessageType -eq [Net.WebSockets.WebSocketMessageType]::Close) { throw 'debugger disconnected' }
                    $memory.Write($buffer, 0, $received.Count)
                    if ($memory.Length -gt 16777216) { throw 'debugger response too large' }
                } while (-not $received.EndOfMessage)
                $message = [Text.Encoding]::UTF8.GetString($memory.ToArray()) | ConvertFrom-Json
            } finally { $memory.Dispose() }
            if ($message.id -eq $requestId) {
                if ($message.error) { throw 'debugger command failed' }
                return $message.result
            }
        }
        } finally { $cancellation.Dispose() }
    }
    $expression = @'
(async()=>{const bdstoken=window.templateData?.bdstoken;if(typeof bdstoken!=="string"||!bdstoken)return null;const r=await fetch("/youai/user/v1/getuinfo?clienttype=70&bdstoken="+encodeURIComponent(bdstoken),{credentials:"include",headers:{"X-Requested-With":"XMLHttpRequest"}});if(!r.ok)return null;const text=await r.text();let obj;try{obj=JSON.parse(text)}catch{return null}if(obj.errno!==0)return null;const m=text.match(/"youa_id"\s*:\s*(?:"([^"\r\n]+)"|(\d+))/);if(!m)return null;return {bdstoken,account_id:m[1]||m[2]};})()
'@
    $deadline = [DateTime]::UtcNow.AddMinutes(5)
    do {
        if (-not (Test-DedicatedBrowser)) { Start-Sleep -Milliseconds 500; continue }
        $targets = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/json/list" -TimeoutSec 5 -Proxy $null
        $target = @($targets | Where-Object { $_.type -eq 'page' -and $_.url -match '^https://photo\.baidu\.com(?:/|$)' }) | Select-Object -First 1
        if (-not $target) { Start-Sleep -Seconds 1; continue }
        $socket = Open-Cdp $target.webSocketDebuggerUrl
        try {
            $result = Invoke-Cdp $socket 'Runtime.evaluate' @{ expression = $expression; awaitPromise = $true; returnByValue = $true }
            $value = $result.result.value
            if ($value -and $value.bdstoken -and $value.account_id) {
                $cookies = Invoke-Cdp $socket 'Network.getCookies' @{ urls = @('https://photo.baidu.com/') }
                $cookie = (@($cookies.cookies | ForEach-Object { "$($_.name)=$($_.value)" }) -join '; ')
                if ([string]::IsNullOrEmpty($cookie)) { throw 'authenticated cookies unavailable' }
                @{ cookie = $cookie; bdstoken = [string]$value.bdstoken; account_id = [string]$value.account_id } | ConvertTo-Json -Compress
                exit 0
            }
        } finally { $socket.Dispose() }
        Start-Sleep -Seconds 1
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'login timeout'
} catch {
    # Do not print exceptions: CDP and HTTP exceptions can contain credentials or URLs.
    [Console]::Error.WriteLine('Dedicated browser login failed; verify the Yike Edge profile and local debugging port.')
    exit 1
}
