param(
    [int]$Port,
    [string]$LogPath
)
# Emulates a connected Studio client for rojo-bench.ps1. Rojo 7.x streams
# patches over a msgpack WebSocket at /api/socket/{cursor}; holding the socket
# open makes the serve session compute and deliver patches like it would for
# Studio. The frames aren't decoded, consuming them is what matters.
"subscriber starting against ws://localhost:$Port/api/socket/0" | Out-File $LogPath -Encoding utf8
while ($true) {
    $ws = New-Object System.Net.WebSockets.ClientWebSocket
    $ct = [System.Threading.CancellationToken]::None
    try {
        $uri = [Uri]"ws://localhost:$Port/api/socket/0"
        $ws.ConnectAsync($uri, $ct).GetAwaiter().GetResult()
        "$(Get-Date -Format 'HH:mm:ss.fff') connected" | Add-Content $LogPath
        $buf = New-Object byte[] 262144
        $seg = New-Object System.ArraySegment[byte] -ArgumentList @(,$buf)
        while ($ws.State -eq 'Open') {
            $total = 0
            do {
                $res = $ws.ReceiveAsync($seg, $ct).GetAwaiter().GetResult()
                $total += $res.Count
            } until ($res.EndOfMessage)
            if ($res.MessageType -eq 'Close') { break }
            "$(Get-Date -Format 'HH:mm:ss.fff') patch message ($total bytes)" | Add-Content $LogPath
        }
        "$(Get-Date -Format 'HH:mm:ss.fff') socket closed (state $($ws.State))" | Add-Content $LogPath
    } catch {
        "$(Get-Date -Format 'HH:mm:ss.fff') socket error: $($_.Exception.GetBaseException().Message)" | Add-Content $LogPath
        Start-Sleep -Milliseconds 500
    } finally {
        $ws.Dispose()
    }
}
