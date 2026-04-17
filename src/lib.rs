//! `RustyHip` is a lambda front end providing `SQLite`-like database over S3

pub mod settings;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Example library function — replace with your own API.
#[must_use]
pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greet_uses_name() {
        assert_eq!(greet("world"), "Hello, world!");
    }
}
