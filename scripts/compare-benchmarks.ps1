param(
    [string]$BaseRef = 'main',
    [string]$CandidateRef = 'HEAD',
    [string]$OutputPath = '',
    [string]$NameFilter = ''
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
& cargo run --quiet --manifest-path (Join-Path $repo 'scripts/bench-compare/Cargo.toml') -- $repo $BaseRef $CandidateRef $OutputPath $NameFilter
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
