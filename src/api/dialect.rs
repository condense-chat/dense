//! The upstream API dialects the condense proxy speaks. A dialect decides the
//! condense route (`<api>/anthropic`); the actual upstream endpoint is a
//! base-URL detail, not a dialect. A concrete dialect is a zero-sized marker.

use crate::config::Config;

pub trait Dialect {
    /// The full condense base URL a tool points at for this dialect.
    fn base_url(&self, cfg: &Config) -> String {
        format!("{}/{}", cfg.api_base_url, self.route())
    }

    /// The condense route segment for this dialect.
    fn route(&self) -> &'static str;
}

pub struct Anthropic;

impl Dialect for Anthropic {
    fn route(&self) -> &'static str {
        "anthropic"
    }
}
