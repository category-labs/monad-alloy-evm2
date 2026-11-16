//! Emit native debugger results for comparison with the installed WASM package.
use monad_evm2js::Engine;
#[path = "../tests/support/debugging.rs"]
mod debugging;
fn main() {
    println!("{}", debugging::report());
}
