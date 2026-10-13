//! Spotifast desktop command.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod entrypoint;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> eframe::Result<()> {
    #[cfg(feature = "profiling")]
    let _profiler = spotifast::profiling::start();
    entrypoint::run()
}
