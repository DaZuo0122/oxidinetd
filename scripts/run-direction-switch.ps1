param([string]$OutputPath = 'target/direction-switch-matrix.md')

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Push-Location $repo
try {
    & cargo build --release --bin oi --example direction_switch_matrix
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & cargo run --release --example direction_switch_matrix -- $OutputPath
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}
