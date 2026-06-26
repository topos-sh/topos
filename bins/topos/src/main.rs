//! `topos` — the client binary. A thin composition root over the library; all logic lives in the lib so
//! it is unit-testable without a process.

fn main() -> std::process::ExitCode {
    topos::run()
}
