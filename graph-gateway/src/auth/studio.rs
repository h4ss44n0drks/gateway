use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::bail;
use eventuals::{Eventual, Ptr};

use crate::subgraph_studio::{APIKey, QueryStatus};
use crate::topology::Deployment;

use super::common::{are_deployments_authorized, are_subgraphs_authorized, is_domain_authorized};

/// Errors that may occur when parsing a Studio API key.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid length: {0}")]
    InvalidLength(usize),

    #[error("invalid format: {0}")]
    InvalidHex(faster_hex::Error),
}

/// Parses a Studio API key from a string.
///
/// The API key is expected to be a 32-character hex string.
pub fn parse_studio_api_key(value: &str) -> Result<[u8; 16], ParseError> {
    if value.len() != 32 {
        return Err(ParseError::InvalidLength(value.len()));
    }

    let mut buf = [0_u8; 16];
    faster_hex::hex_decode(value.as_bytes(), &mut buf).map_err(ParseError::InvalidHex)?;
    Ok(buf)
}

/// App state (a.k.a [Context](crate::client_query::Context)) sub-state.
pub struct AuthHandler {
    /// A map between Studio auth bearer token string and the Studio [ApiKey].
    ///
    /// API keys are fetched periodically (every 30s) from the Studio API by the gateway using the
    /// [`subgraph_studio` client](crate::subgraph_studio::Client).
    pub(super) studio_keys: Eventual<Ptr<HashMap<String, Arc<APIKey>>>>,

    /// Special API keys that don't require payment.
    ///
    /// An API key is considered special when does not require payment and is not subsidized, i.e., these
    /// keys won't be rejected due to non-payment.
    pub(super) special_api_keys: Arc<HashSet<String>>,

    /// Whether all API keys require payment.
    ///
    /// This is used to disable the payment requirement on testnets. If this is `true`, then all API keys require
    /// payment, unless they are subsidized or special.
    pub(super) api_key_payment_required: bool,
}

impl AuthHandler {
    /// Get the Studio API key associated with the given bearer token string.
    pub fn get_api_key(&self, token: &str) -> Option<Arc<APIKey>> {
        self.studio_keys.value_immediate()?.get(token).cloned()
    }

    /// Whether all API keys require payment.
    ///
    /// This is used to disable the payment requirement on testnets. If this is `true`, then all API keys require
    /// payment, unless they are subsidized or special.
    pub fn is_payment_required(&self) -> bool {
        self.api_key_payment_required
    }

    /// Check if the given API key is a special key.
    ///
    /// An API key is considered special when does not require payment and is not subsidized, i.e., these
    /// keys won't be rejected due to non-payment.
    pub fn is_special_key(&self, api_key: &APIKey) -> bool {
        self.special_api_keys.contains(&api_key.key)
    }
}

pub fn parse_bearer_token(auth: &AuthHandler, token: &str) -> anyhow::Result<Arc<APIKey>> {
    // Check if the bearer token is a valid 32 hex digits key
    if parse_studio_api_key(token).is_err() {
        return Err(anyhow::anyhow!("Invalid api key format"));
    }

    // Retrieve the API Key associated with the bearer token
    auth.get_api_key(token)
        .ok_or_else(|| anyhow::anyhow!("API key not found"))
}

pub async fn check_token(
    auth: &AuthHandler,
    api_key: &Arc<APIKey>,
    deployments: &[Arc<Deployment>],
    domain: &str,
) -> anyhow::Result<()> {
    // Enforce the API key payment status, unless it's being subsidized.
    if auth.is_payment_required() && !api_key.is_subsidized && !auth.is_special_key(api_key) {
        match api_key.query_status {
            QueryStatus::Active => (),
            QueryStatus::Inactive => bail!("Querying not activated yet; make sure to add some GRT to your balance in the studio"),
            QueryStatus::ServiceShutoff => bail!("Payment required for subsequent requests for this API key"),
        };
    }

    // Check deployment allowlist
    let allowed_deployments = &api_key.deployments;

    tracing::debug!(?allowed_deployments);
    if !are_deployments_authorized(allowed_deployments, deployments) {
        return Err(anyhow::anyhow!("Deployment not authorized by user"));
    }

    // Check subgraph allowlist
    let allowed_subgraphs = &api_key.subgraphs;

    tracing::debug!(?allowed_subgraphs);
    if !are_subgraphs_authorized(allowed_subgraphs, deployments) {
        return Err(anyhow::anyhow!("Subgraph not authorized by user"));
    }

    // Check domain allowlist
    let allowed_domains = &api_key
        .domains
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>();

    tracing::debug!(?allowed_domains);
    if !is_domain_authorized(allowed_domains, domain) {
        return Err(anyhow::anyhow!("Domain not authorized by user"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parser {
        use assert_matches::assert_matches;

        use super::{parse_studio_api_key, ParseError};

        #[test]
        fn parse_invalid_length_studio_api_key() {
            //* Given
            let api_key = "0123456789abcdef0123456789abcde";

            //* When
            let result = parse_studio_api_key(api_key);

            //* Then
            assert_matches!(result, Err(ParseError::InvalidLength(31)));
        }

        #[test]
        fn parse_invalid_format_studio_api_key() {
            //* Given
            let api_key = "abcdefghijklmnopqrstuvwxyz123456";

            //* When
            let result = parse_studio_api_key(api_key);

            //* Then
            assert_matches!(result, Err(ParseError::InvalidHex(_)));
        }

        #[test]
        fn parse_valid_studio_api_key() {
            //* Given
            let api_key = "0123456789abcdef0123456789abcdef";

            //* When
            let result = parse_studio_api_key(api_key);

            //* Then
            assert_matches!(result, Ok(key) => {
                assert_eq!(
                    key,
                    [
                        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                    ]
                );
            });
        }
    }
}