# Allows Ho My Pod to receive what HomePods send back (UDP clock sync and
# retransmit requests, TCP remote control commands), from the local network only.
# Must run elevated. Usage: allow-firewall.ps1 <exe path> [<exe path>...]
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Programs)

foreach ($p in $Programs) {
    $full = [System.IO.Path]::GetFullPath($p)
    # Include the parent folder so debug/release builds get distinct rules
    $parent = Split-Path (Split-Path $full -Parent) -Leaf
    $name = "Ho My Pod (AirPlay) - $parent\" + [System.IO.Path]::GetFileName($full)
    Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -DisplayName $name -Direction Inbound -Program $full `
        -Action Allow -RemoteAddress LocalSubnet -Profile Any | Out-Null
    Write-Output "Allowed: $full"
}
