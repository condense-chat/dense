//! The upstream API dialects the condense proxy speaks. A dialect decides the
//! condense route (`<api>/anthropic`); the actual upstream endpoint is a
//! base-URL detail, not a dialect.

use crate::config::Config;

#[derive(Clone, Copy)]
pub enum Dialect {
    Anthropic,
    OpenAi,
}

impl Dialect {
    /// The condense route segment for this dialect.
    pub fn route(self) -> &'static str {
        match self {
            Dialect::Anthropic => "anthropic",
            Dialect::OpenAi => "openai",
        }
    }

    /// The full condense base URL a tool points at for this dialect.
    pub fn base_url(self, cfg: &Config) -> String {
        format!("{}/{}", cfg.api_base_url, self.route())
    }
}
