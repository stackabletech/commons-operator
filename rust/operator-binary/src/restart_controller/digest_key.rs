//! The secret key of the content digests tracked by the StatefulSet restarter.
//!
//! The digests end up in Pod template annotations, which anyone who can read a StatefulSet or its
//! Pods can see. Without a secret key, a digest would let them verify guesses of a Secret's
//! content offline. Each digest is also bound to the uid of its object (see
//! [`crate::restart_controller::content_digest`]), so that the operator cannot be used to verify
//! such guesses either. The key lives in a Secret in the operator namespace, so that it survives
//! operator restarts: a new key changes every digest and rolling-restarts every StatefulSet managed
//! by the restarter.

use std::fmt;

use hmac::{Hmac, KeyInit};
use sha2::Sha256;
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    builder::meta::ObjectMetaBuilder,
    client::Client,
    k8s_openapi::{ByteString, api::core::v1::Secret},
    kvp::Label,
};
use tokio::sync::OnceCell;

/// Name of the Secret holding the [`DigestKey`].
const SECRET_NAME: &str = "commons-operator-restarter-digest-key";

/// Entry of the Secret data holding the [`DigestKey`].
const SECRET_DATA_KEY: &str = "key";

/// Length of the [`DigestKey`] in bytes.
const KEY_LENGTH: usize = 32;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to load or create the digest key Secret {SECRET_NAME:?}"))]
    LoadOrCreateSecret {
        source: stackable_operator::client::Error,
    },

    #[snafu(display(
        "the digest key Secret {SECRET_NAME:?} does not hold a {KEY_LENGTH} byte key in {SECRET_DATA_KEY:?}, delete it to generate a new key (this rolling-restarts every StatefulSet managed by the restarter)"
    ))]
    InvalidKey,
}

/// The secret key of the content digests.
#[derive(Clone)]
pub struct DigestKey([u8; KEY_LENGTH]);

impl DigestKey {
    pub const fn new(bytes: [u8; KEY_LENGTH]) -> Self {
        Self(bytes)
    }

    /// Returns a new HMAC-SHA256 keyed with this key.
    pub fn mac(&self) -> Hmac<Sha256> {
        Hmac::new_from_slice(&self.0).expect("HMAC must accept keys of any length")
    }

    fn generate() -> Self {
        Self::new(rand::random())
    }

    fn from_secret(secret: &Secret) -> Result<Self, Error> {
        let bytes = secret
            .data
            .as_ref()
            .and_then(|data| data.get(SECRET_DATA_KEY))
            .and_then(|value| <[u8; KEY_LENGTH]>::try_from(value.0.as_slice()).ok())
            .context(InvalidKeySnafu)?;

        Ok(Self::new(bytes))
    }

    fn to_secret(&self, namespace: &str) -> Secret {
        Secret {
            metadata: ObjectMetaBuilder::new()
                .name(SECRET_NAME)
                .namespace(namespace)
                .with_label(Label::stackable_vendor())
                .build(),
            data: Some([(SECRET_DATA_KEY.to_owned(), ByteString(self.0.to_vec()))].into()),
            immutable: Some(true),
            ..Secret::default()
        }
    }
}

/// Redacted, so that the key never ends up in logs.
impl fmt::Debug for DigestKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DigestKey(<redacted>)")
    }
}

/// Loads the [`DigestKey`] on first use, creating its Secret if it does not exist yet.
///
/// Loading lazily keeps the operator startup independent of the Secret: if it cannot be loaded or
/// created, only the reconciles that need a digest fail, and they are retried.
pub struct DigestKeyStore {
    client: Client,
    namespace: String,
    key: OnceCell<DigestKey>,
}

impl DigestKeyStore {
    pub fn new(client: Client, namespace: String) -> Self {
        Self {
            client,
            namespace,
            key: OnceCell::new(),
        }
    }

    /// Returns the key, loading or creating its Secret in the operator namespace on first use.
    ///
    /// A failed attempt is not cached, the next call tries again.
    pub async fn get(&self) -> Result<&DigestKey, Error> {
        self.key
            .get_or_try_init(|| async {
                let secret = self
                    .client
                    .create_if_missing(&DigestKey::generate().to_secret(&self.namespace))
                    .await
                    .context(LoadOrCreateSecretSnafu)?;
                DigestKey::from_secret(&secret)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips_through_its_secret() {
        let key = DigestKey::new([7; KEY_LENGTH]);
        let secret = key.to_secret("operator-namespace");

        assert_eq!(secret.metadata.name.as_deref(), Some(SECRET_NAME));
        assert_eq!(
            secret.metadata.namespace.as_deref(),
            Some("operator-namespace")
        );
        assert_eq!(secret.immutable, Some(true));
        assert_eq!(
            DigestKey::from_secret(&secret)
                .expect("a Secret built from a key should hold a valid key")
                .0,
            key.0
        );
    }

    #[test]
    fn secret_without_valid_key_is_rejected() {
        let short_key = Secret {
            data: Some([(SECRET_DATA_KEY.to_owned(), ByteString(vec![7; 16]))].into()),
            ..Secret::default()
        };
        let wrong_entry = Secret {
            data: Some([("other".to_owned(), ByteString(vec![7; KEY_LENGTH]))].into()),
            ..Secret::default()
        };

        for secret in [Secret::default(), short_key, wrong_entry] {
            assert!(matches!(
                DigestKey::from_secret(&secret),
                Err(Error::InvalidKey)
            ));
        }
    }

    #[test]
    fn debug_output_redacts_the_key() {
        assert_eq!(
            format!("{:?}", DigestKey::new([7; KEY_LENGTH])),
            "DigestKey(<redacted>)"
        );
    }
}
