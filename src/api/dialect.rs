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

/// A tool that speaks several dialects at once — one condense provider per
/// dialect (OpenCode). Composes the single-dialect markers rather than
/// restating routes; each entry carries the dialect's route and base URL.
pub trait MultiDialect {
    fn dialects(&self, cfg: &Config) -> Vec<DialectRoute>;
}

/// Every dialect condense speaks — the full multi-provider set.
pub struct AllDialects;

pub struct Anthropic;

/// One resolved dialect of a [`MultiDialect`]: its route plus condense base URL.
pub struct DialectRoute {
    pub base_url: String,
    pub route: &'static str,
}

pub struct OpenAi;

impl Dialect for Anthropic {
    fn route(&self) -> &'static str {
        "anthropic"
    }
}

impl Dialect for OpenAi {
    fn route(&self) -> &'static str {
        "openai"
    }
}

impl MultiDialect for AllDialects {
    fn dialects(&self, cfg: &Config) -> Vec<DialectRoute> {
        [&Anthropic as &dyn Dialect, &OpenAi]
            .into_iter()
            .map(|d| DialectRoute {
                base_url: d.base_url(cfg),
                route: d.route(),
            })
            .collect()
    }
}
