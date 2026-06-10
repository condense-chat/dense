//! Fetch an environment profile descriptor from a cli host's `/profile`.

use backon::{ExponentialBuilder, Retryable};

use crate::Result;
use crate::api::Api;
use crate::profile::Profile;

/// `GET <base>/profile` → the environment descriptor. Idempotent, so a couple
/// of retries smooth over transient network blips.
pub async fn fetch(base: &str) -> Result<Profile> {
    let api = Api::anonymous(base)?;
    (|| api.get_json::<Profile>("/profile"))
        .retry(ExponentialBuilder::default().with_max_times(2))
        .await
}
