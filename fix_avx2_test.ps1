$ErrorActionPreference = 'Stop'
$cargo = "C:\Users\runneradmin\.cargo\bin\cargo.exe"
if (-Not (Test-Path $cargo)) {
    throw "cargo not found at $cargo"
}
& $cargo test --release --all-features -- --nocapture
