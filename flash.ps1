<#
.SYNOPSIS
    Build, flash and monitor the CYD Weather Station firmware on Windows.

.DESCRIPTION
    The Cheap Yellow Display (ESP32-2432S028R) exposes a CH340 USB-serial bridge
    on its micro-USB port. Connect the board via that data port before running.

.PARAMETER Port
    Explicit serial port (e.g. COM5). If omitted, espflash auto-detects.

.PARAMETER Baud
    Flashing baud rate (default 460800).

.EXAMPLE
    ./flash.ps1
    ./flash.ps1 -Port COM5
    ./flash.ps1 -Baud 921600
#>
param(
    [string]$Port = "",
    [int]$Baud = 460800
)

$ErrorActionPreference = "Stop"

# Load the ESP Rust toolchain environment if espup created it.
$exportPs = Join-Path $HOME "export-esp.ps1"
if (Test-Path $exportPs) {
    . $exportPs
}

if (-not (Get-Command espflash -ErrorAction SilentlyContinue)) {
    Write-Error "'espflash' not found. Install it with: cargo install espflash"
    exit 1
}

$flashArgs = @("espflash", "flash", "--release", "--baud", "$Baud")
if ($Port -ne "") {
    Write-Host "Using serial port: $Port"
    $flashArgs += @("--port", $Port)
}
else {
    Write-Host "No port specified; letting espflash auto-detect."
}
$flashArgs += "--monitor"

Write-Host "Building (release) and flashing at $Baud baud..."
& cargo @flashArgs
