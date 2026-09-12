#Requires -RunAsAdministrator
# Allow filele through Windows Firewall (TCP 53317-53318, UDP 53319)
$exe = Join-Path $PSScriptRoot "target\release\filele.exe"
if (!(Test-Path $exe)) { $exe = Join-Path $PSScriptRoot "filele.exe" }
Write-Host "Allowing $exe ..."
New-NetFirewallRule -DisplayName "filele TCP" -Direction Inbound -Program $exe -Action Allow -Protocol TCP -LocalPort 53317-53318 -ErrorAction SilentlyContinue | Out-Null
New-NetFirewallRule -DisplayName "filele UDP discover" -Direction Inbound -Program $exe -Action Allow -Protocol UDP -LocalPort 53319 -ErrorAction SilentlyContinue | Out-Null
# Also open ports for any program (helps native Rust sockets on some setups)
New-NetFirewallRule -DisplayName "filele TCP ports" -Direction Inbound -Action Allow -Protocol TCP -LocalPort 53317-53318 -ErrorAction SilentlyContinue | Out-Null
New-NetFirewallRule -DisplayName "filele UDP port" -Direction Inbound -Action Allow -Protocol UDP -LocalPort 53319 -ErrorAction SilentlyContinue | Out-Null
Write-Host "Firewall rules added. Verify with: Get-NetFirewallRule -DisplayName 'filele*'"
