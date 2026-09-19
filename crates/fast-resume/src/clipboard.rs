//! System clipboard access.
//!
//! `arboard` has no Android backend — its platform modules are only cfg'd in
//! for Windows, macOS and Linux, so the crate does not compile at all on
//! `target_os = "android"`. This module re-exports `arboard` on every other
//! platform and provides a no-op stand-in on Android, where the clipboard is
//! not reachable from a plain terminal process.

#[cfg(not(target_os = "android"))]
pub use arboard::*;

#[cfg(target_os = "android")]
mod android {
    use std::borrow::Cow;
    use std::fmt;

    /// Clipboard access is unavailable on this platform.
    #[derive(Debug, Clone, Copy)]
    pub struct Error;

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("clipboard is not available on Android/Termux")
        }
    }

    impl std::error::Error for Error {}

    /// No-op stand-in for `arboard::Clipboard`.
    pub struct Clipboard;

    impl Clipboard {
        pub fn new() -> Result<Self, Error> {
            Ok(Clipboard)
        }

        pub fn set_text<'a, T: Into<Cow<'a, str>>>(&mut self, _text: T) -> Result<(), Error> {
            Err(Error)
        }

        pub fn get_text(&mut self) -> Result<String, Error> {
            Err(Error)
        }
    }
}

#[cfg(target_os = "android")]
pub use android::*;
