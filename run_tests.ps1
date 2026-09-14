$ErrorActionPreference = 'Stop'
$cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
$cargo = if ($cargoCommand) {
    $cargoCommand.Source
} elseif (Test-Path 'C:\Users\runneradmin\.cargo\bin\cargo.exe') {
    'C:\Users\runneradmin\.cargo\bin\cargo.exe'
} else {
    throw 'cargo not found; install Rust or add cargo to PATH'
}
& $cargo test --release --all-features -- '--nocapture'
