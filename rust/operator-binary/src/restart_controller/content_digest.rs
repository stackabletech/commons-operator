//! Digests over the content of ConfigMaps and Secrets, for the `data-hash` change tracking
//! mechanism of the StatefulSet restarter.
//!
//! The restarter only watches the metadata of ConfigMaps and Secrets. The content of an object is
//! fetched on demand, only when a StatefulSet using `data-hash` references it, and only once per
//! resourceVersion: the [`DigestCache`] keeps the digest, never the content.
//!
//! The digests are keyed with a [`DigestKey`] and bound to the uid of their object, so that anyone
//! who can read them in the Pod template annotations learns nothing about the content beyond
//! whether it changed.

use std::{
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    sync::{Mutex, MutexGuard},
};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use stackable_operator::{
    k8s_openapi::{
        api::core::v1::{ConfigMap, Secret},
        apimachinery::pkg::apis::meta::v1::ObjectMeta,
    },
    kube::Resource,
};

use crate::restart_controller::digest_key::DigestKey;

/// An object whose content can be digested.
pub trait ContentDigest: Resource<DynamicType = ()> {
    /// Keyed digest over the parts of the object that a Pod can read, bound to the object's `uid`.
    fn content_digest(&self, uid: &str, digest_key: &DigestKey) -> String;
}

impl ContentDigest for ConfigMap {
    fn content_digest(&self, uid: &str, digest_key: &DigestKey) -> String {
        let mut mac = object_mac(uid, digest_key);
        mac.update(b"configmap/data");
        for (key, value) in self.data.iter().flatten() {
            update_entry(&mut mac, key, value.as_bytes());
        }
        mac.update(b"configmap/binaryData");
        for (key, value) in self.binary_data.iter().flatten() {
            update_entry(&mut mac, key, &value.0);
        }
        finish_digest(mac)
    }
}

impl ContentDigest for Secret {
    fn content_digest(&self, uid: &str, digest_key: &DigestKey) -> String {
        let mut mac = object_mac(uid, digest_key);
        mac.update(b"secret/data");
        for (key, value) in self.data.iter().flatten() {
            update_entry(&mut mac, key, &value.0);
        }
        finish_digest(mac)
    }
}

/// Returns a new MAC keyed with `digest_key` and bound to the object with the given `uid`.
///
/// Binding to the uid keeps the digests of different objects apart, even with identical content.
/// Otherwise anyone able to create an object could have the operator digest guesses of another
/// object's content and compare the results.
fn object_mac(uid: &str, digest_key: &DigestKey) -> Hmac<Sha256> {
    let mut mac = digest_key.mac();
    update_entry(&mut mac, "uid", uid.as_bytes());
    mac
}

/// Adds one key/value pair to `mac`, length-prefixed.
fn update_entry(mac: &mut Hmac<Sha256>, key: &str, value: &[u8]) {
    mac.update(&(key.len() as u64).to_be_bytes());
    mac.update(key.as_bytes());
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

/// Truncates the digest to 128 bits and hex-encodes it.
fn finish_digest(mac: Hmac<Sha256>) -> String {
    mac.finalize().into_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The content digest of one object at one resourceVersion.
///
/// Deliberately holds no content, so no Secret data stays in memory after hashing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedDigest {
    pub uid: String,
    pub resource_version: String,
    pub digest: String,
}

/// Namespace and name of a cached object.
type CacheKey = (String, String);

/// Content digests of the objects of kind `K` referenced by StatefulSets using `data-hash`, keyed
/// by namespace and name.
///
/// An entry is replaced when the uid or resourceVersion changes, and removed when a fetch finds
/// the object gone. Entries of objects deleted without a later fetch stay until the operator
/// restarts. They cannot be served for a recreated object, which has a new uid, and are only a
/// few hundred bytes each, so they are not tracked any further.
#[derive(Debug)]
pub struct DigestCache<K> {
    entries: Mutex<HashMap<CacheKey, CachedDigest>>,
    kind: PhantomData<fn() -> K>,
}

impl<K> Default for DigestCache<K> {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            kind: PhantomData,
        }
    }
}

impl<K: ContentDigest> DigestCache<K> {
    /// Returns the digest of the object described by `metadata`, as seen by the metadata watch.
    ///
    /// The cached digest is used if it was computed for the same uid and resourceVersion,
    /// otherwise the object is fetched by calling `fetch` with its name and namespace, and its
    /// digest cached. Returns `None` if `metadata` lacks a name or namespace, or `fetch` does not
    /// find the object.
    ///
    /// All calls on one cache must pass the same `digest_key`, as the cached digests are not keyed
    /// by it.
    pub async fn get_or_fetch<'a, E, Fut>(
        &self,
        metadata: &'a ObjectMeta,
        digest_key: &DigestKey,
        fetch: impl FnOnce(&'a str, &'a str) -> Fut,
    ) -> Result<Option<CachedDigest>, E>
    where
        Fut: Future<Output = Result<Option<K>, E>>,
    {
        let (Some(namespace), Some(name)) = (&metadata.namespace, &metadata.name) else {
            return Ok(None);
        };
        let cache_key = (namespace.clone(), name.clone());

        if let Some(cached) = self.lock().get(&cache_key)
            && Some(&cached.uid) == metadata.uid.as_ref()
            && Some(&cached.resource_version) == metadata.resource_version.as_ref()
        {
            return Ok(Some(cached.clone()));
        }

        tracing::debug!(
            k8s.object.namespace = namespace,
            k8s.object.name = name,
            "Fetching object to digest its content"
        );
        let Some(object) = fetch(name, namespace).await? else {
            self.lock().remove(&cache_key);
            return Ok(None);
        };
        let (Some(uid), Some(resource_version)) = (
            object.meta().uid.clone(),
            object.meta().resource_version.clone(),
        ) else {
            return Ok(None);
        };
        let cached = CachedDigest {
            digest: object.content_digest(&uid, digest_key),
            uid,
            resource_version,
        };
        self.lock().insert(cache_key, cached.clone());

        Ok(Some(cached))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<CacheKey, CachedDigest>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, convert::Infallible};

    use stackable_operator::k8s_openapi::{
        ByteString, apimachinery::pkg::apis::meta::v1::ManagedFieldsEntry,
    };

    use super::*;

    const TEST_KEY: DigestKey = DigestKey::new([42; 32]);

    const TEST_UID: &str = "f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c";

    fn config_map(data: &[(&str, &str)]) -> ConfigMap {
        ConfigMap {
            data: Some(
                data.iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            ),
            ..ConfigMap::default()
        }
    }

    fn metadata(uid: &str, resource_version: &str) -> ObjectMeta {
        ObjectMeta {
            name: Some("my-config".to_owned()),
            namespace: Some("my-namespace".to_owned()),
            uid: Some(uid.to_owned()),
            resource_version: Some(resource_version.to_owned()),
            ..ObjectMeta::default()
        }
    }

    fn stored(uid: &str, resource_version: &str, data: &[(&str, &str)]) -> ConfigMap {
        ConfigMap {
            metadata: metadata(uid, resource_version),
            ..config_map(data)
        }
    }

    /// Changing the digest function rolling-restarts every StatefulSet using `data-hash`.
    #[test]
    fn digest_is_stable_across_releases() {
        let binary_config_map = ConfigMap {
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..ConfigMap::default()
        };
        let secret = Secret {
            data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..Secret::default()
        };

        assert_eq!(
            config_map(&[("foo", "bar")]).content_digest(TEST_UID, &TEST_KEY),
            "aec6a1a9a372bda7f4b7a600d631186f"
        );
        assert_eq!(
            binary_config_map.content_digest(TEST_UID, &TEST_KEY),
            "c2efb2422a076038e7e3affa23d7ffb1"
        );
        assert_eq!(
            secret.content_digest(TEST_UID, &TEST_KEY),
            "646f7311f8401d03267d18cf5419ba32"
        );
    }

    #[test]
    fn digest_ignores_metadata() {
        let mut changed = config_map(&[("foo", "bar")]);
        changed.metadata = ObjectMeta {
            resource_version: Some("12345".to_owned()),
            labels: Some([("cost-center".to_owned(), "irrelevant".to_owned())].into()),
            annotations: Some([("probe".to_owned(), "1".to_owned())].into()),
            managed_fields: Some(vec![ManagedFieldsEntry {
                manager: Some("third-party-controller".to_owned()),
                ..ManagedFieldsEntry::default()
            }]),
            ..ObjectMeta::default()
        };

        assert_eq!(
            config_map(&[("foo", "bar")]).content_digest(TEST_UID, &TEST_KEY),
            changed.content_digest(TEST_UID, &TEST_KEY)
        );
    }

    /// Each pair would collide without the key, the uid binding, the length prefixes or the field
    /// prefixes.
    #[test]
    fn digest_distinguishes_content() {
        let binary = ConfigMap {
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..ConfigMap::default()
        };
        let text = config_map(&[("foo", "bar")]);

        for (left, right) in [
            (
                text.content_digest(TEST_UID, &TEST_KEY),
                text.content_digest(TEST_UID, &DigestKey::new([43; 32])),
            ),
            (
                text.content_digest(TEST_UID, &TEST_KEY),
                text.content_digest("0b6f1a52-2d1e-4c43-8c55-5ad3d3a4e7f1", &TEST_KEY),
            ),
            (
                config_map(&[("ab", "c")]).content_digest(TEST_UID, &TEST_KEY),
                config_map(&[("a", "bc")]).content_digest(TEST_UID, &TEST_KEY),
            ),
            (
                text.content_digest(TEST_UID, &TEST_KEY),
                binary.content_digest(TEST_UID, &TEST_KEY),
            ),
        ] {
            assert_ne!(left, right);
        }
    }

    /// Each step looks the object up by uid and resourceVersion, with the content a fetch returns
    /// (`None` if the object is gone) and whether the cache has to fetch it.
    #[tokio::test]
    async fn cache_fetches_once_per_uid_and_resource_version() {
        let cache = DigestCache::<ConfigMap>::default();

        for (uid, resource_version, content, fetches) in [
            ("uid-1", "100", Some("bar"), true),
            ("uid-1", "100", Some("bar"), false),
            ("uid-1", "101", Some("baz"), true),
            ("uid-2", "101", Some("qux"), true),
            ("uid-2", "102", None, true),
            ("uid-2", "101", Some("qux"), true),
        ] {
            let fetched = Cell::new(false);
            let object = content.map(|content| stored(uid, resource_version, &[("foo", content)]));
            let cached = cache
                .get_or_fetch(&metadata(uid, resource_version), &TEST_KEY, |_, _| {
                    fetched.set(true);
                    std::future::ready(Ok::<_, Infallible>(object.clone()))
                })
                .await
                .expect("fetching should be infallible");

            assert_eq!(fetched.get(), fetches);
            assert_eq!(
                cached,
                object.map(|object| CachedDigest {
                    uid: uid.to_owned(),
                    resource_version: resource_version.to_owned(),
                    digest: object.content_digest(uid, &TEST_KEY),
                })
            );
        }
    }
}
