$ErrorActionPreference = 'Stop'
$cargo = 'C:\Users\runneradmin\.cargo\bin\cargo.exe'
if (-Not (Test-Path $cargo)) { throw 'cargo not found' }
& $cargo test --release --all-features -- '--nocapture'
