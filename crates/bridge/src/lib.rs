pub mod backup;
pub mod body_cache;
pub mod bridge;
pub mod config;
pub mod event_handler;
mod fs_private;
mod governed_client;
pub mod imap;
pub mod mail;
pub mod mcp;
mod net;
pub mod smtp;
pub mod store;
pub mod sync;
pub mod tls;
pub mod tuta;
pub mod util;

/// The version people see: the release tag without its `v` when the release
/// job sets `TUTABRIDGE_VERSION`, the crate version with a `-dev` suffix for
/// every other build, so a local build never reads like a release.
pub const VERSION: &str = match option_env!("TUTABRIDGE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

#[cfg(test)]
mod version_tests {
    #[test]
    fn a_build_without_the_release_variable_is_marked_dev() {
        assert!(super::VERSION.starts_with(|c: char| c.is_ascii_digit()));
        if option_env!("TUTABRIDGE_VERSION").is_none_or(str::is_empty) {
            assert_eq!(super::VERSION, concat!(env!("CARGO_PKG_VERSION"), "-dev"));
        }
    }
}
