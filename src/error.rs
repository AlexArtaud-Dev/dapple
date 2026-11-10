//! One error type: a message meant for the person at the terminal.

use std::fmt;

#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

macro_rules! from {
    ($($source:ty),*) => {$(
        impl From<$source> for Error {
            fn from(error: $source) -> Self {
                Error(error.to_string())
            }
        }
    )*};
}

from!(
    std::io::Error,
    serde_json::Error,
    roxmltree::Error,
    rustls::Error,
    lexopt::Error
);

/// Builds an [`Error`] like `format!`.
#[macro_export]
macro_rules! fail {
    ($($arg:tt)*) => {
        $crate::error::Error(format!($($arg)*))
    };
}
