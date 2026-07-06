//! insane-cli binary entry point. All real logic lives in `src/lib.rs` (the
//! `insane_cli` library crate) so integration tests under `tests/` can drive
//! it directly (e.g. the real `RateLimiter`, `ApiError`, config precedence)
//! against a mock NIM server. This shim exists only so `cargo build --release`
//! still produces the `insane` binary.

fn main() {
    std::process::exit(insane_cli::main_entry());
}
