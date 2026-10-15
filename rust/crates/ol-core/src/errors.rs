//! Port of core/exceptions.py

use std::fmt;

/// Raised when a required third-party library is not installed.
/// (In the Rust port this is used for features that are not compiled in.)
#[derive(Debug, Clone)]
pub struct DependencyMissing(pub String);

impl fmt::Display for DependencyMissing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DependencyMissing {}

/// Raised when trying to do something you're not authorized to do
#[derive(Debug, Clone)]
pub struct UnauthorizedException(pub String);

impl fmt::Display for UnauthorizedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UnauthorizedException {}
