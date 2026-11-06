param([string]$OutputPath = 'target/network-workload-matrix.md')

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Push-Location $repo
try {
    if ($env:IPERF3 -and -not (Test-Path -LiteralPath $env:IPERF3)) {
        throw "IPERF3 does not exist: $env:IPERF3"
    }
    & cargo build --release --features tcp-profile --bin oi --example network_workload_matrix
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & cargo run --release --features tcp-profile --example network_workload_matrix -- $OutputPath
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}
