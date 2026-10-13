//! Opt-in frame profiling (`--features profiling`). Without the feature every
//! scope compiles to nothing.
//!
//! eframe and egui already mark frames and their own scopes through the
//! `profiling` crate, which the feature routes to puffin, so this module only
//! adds the scope macro and the server a viewer connects to.

#[cfg(feature = "profiling")]
macro_rules! profile_scope {
    ($($arg:tt)*) => {
        ::puffin::profile_scope!($($arg)*)
    };
}

#[cfg(not(feature = "profiling"))]
macro_rules! profile_scope {
    ($($arg:tt)*) => {};
}

pub(crate) use profile_scope;

/// Address `puffin_viewer` connects to (`puffin_viewer --url 127.0.0.1:8585`).
#[cfg(feature = "profiling")]
pub const ADDRESS: &str = "127.0.0.1:8585";

/// Turns scope collection on and serves it on [`ADDRESS`]. Keep the returned
/// server alive for as long as the profile should be available.
#[cfg(feature = "profiling")]
pub fn start() -> Option<puffin_http::Server> {
    puffin::set_scopes_on(true);
    match puffin_http::Server::new(ADDRESS) {
        Ok(server) => {
            log::info!("puffin profiler listening on {ADDRESS}");
            Some(server)
        }
        Err(error) => {
            log::warn!("puffin profiler could not listen on {ADDRESS}: {error}");
            None
        }
    }
}
