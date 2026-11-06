param(
    [string]$OutputPath = 'target/network-workload-matrix.md',
    [switch]$Production
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Push-Location $repo
try {
    if ($env:IPERF3 -and -not (Test-Path -LiteralPath $env:IPERF3)) {
        throw "IPERF3 does not exist: $env:IPERF3"
    }
    [string[]]$featureArgs = @()
    if (-not $Production) { $featureArgs = @('--features', 'tcp-profile') }
    & cargo build --release @featureArgs --bin oi --example network_workload_matrix
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & cargo run --release @featureArgs --example network_workload_matrix -- $OutputPath
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}
