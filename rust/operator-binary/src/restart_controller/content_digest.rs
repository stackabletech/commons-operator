//! Digests over the content of ConfigMaps and Secrets, for the `data-hash` change tracking
//! mechanism of the StatefulSet restarter.
//!
//! The restarter only watches the metadata of ConfigMaps and Secrets. The content of an object is
//! fetched on demand, only when a StatefulSet using `data-hash` references it, and only once per
//! resourceVersion: the [`DigestCache`] keeps the digest, never the content.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Mutex, MutexGuard},
};

use sha2::{Digest, Sha256};
use stackable_operator::{
    k8s_openapi::{
        api::core::v1::{ConfigMap, Secret},
        apimachinery::pkg::apis::meta::v1::ObjectMeta,
    },
    kube::Resource,
};

/// An object whose content can be digested.
pub trait ContentDigest: Resource<DynamicType = ()> {
    /// Digest over the parts of the object that a Pod can read.
    fn content_digest(&self) -> String;
}

impl ContentDigest for ConfigMap {
    fn content_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"configmap/data");
        for (key, value) in self.data.iter().flatten() {
            update_entry(&mut hasher, key, value.as_bytes());
        }
        hasher.update(b"configmap/binaryData");
        for (key, value) in self.binary_data.iter().flatten() {
            update_entry(&mut hasher, key, &value.0);
        }
        finish_digest(hasher)
    }
}

impl ContentDigest for Secret {
    fn content_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"secret/data");
        for (key, value) in self.data.iter().flatten() {
            update_entry(&mut hasher, key, &value.0);
        }
        finish_digest(hasher)
    }
}

/// Adds one key/value pair to `hasher`, length-prefixed.
fn update_entry(hasher: &mut Sha256, key: &str, value: &[u8]) {
    hasher.update((key.len() as u64).to_be_bytes());
    hasher.update(key.as_bytes());
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Truncates the digest to 128 bits and hex-encodes it.
fn finish_digest(hasher: Sha256) -> String {
    hasher.finalize()[..16]
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

/// Content digests of the ConfigMaps or the Secrets referenced by StatefulSets using `data-hash`,
/// keyed by namespace and name.
///
/// An entry is replaced when the uid or resourceVersion changes, and removed when a fetch finds
/// the object gone. Entries of objects deleted without a later fetch stay until the operator
/// restarts. They cannot be served for a recreated object, which has a new uid, and are only a
/// few hundred bytes each, so they are not tracked any further.
#[derive(Debug, Default)]
pub struct DigestCache {
    entries: Mutex<HashMap<CacheKey, CachedDigest>>,
}

impl DigestCache {
    /// Returns the digest of the object described by `metadata`, as seen by the metadata watch.
    ///
    /// The cached digest is used if it was computed for the same uid and resourceVersion,
    /// otherwise the object is fetched by calling `fetch` with its name and namespace, and its
    /// digest cached. Returns `None` if `metadata` lacks a name or namespace, or `fetch` does not
    /// find the object.
    pub async fn get_or_fetch<'a, K, E, Fut>(
        &self,
        metadata: &'a ObjectMeta,
        fetch: impl FnOnce(&'a str, &'a str) -> Fut,
    ) -> Result<Option<CachedDigest>, E>
    where
        K: ContentDigest,
        Fut: Future<Output = Result<Option<K>, E>>,
    {
        let (Some(namespace), Some(name)) = (&metadata.namespace, &metadata.name) else {
            return Ok(None);
        };
        let key = (namespace.clone(), name.clone());

        if let Some(cached) = self.lock().get(&key)
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
            self.lock().remove(&key);
            return Ok(None);
        };
        let (Some(uid), Some(resource_version)) = (
            object.meta().uid.clone(),
            object.meta().resource_version.clone(),
        ) else {
            return Ok(None);
        };
        let cached = CachedDigest {
            uid,
            resource_version,
            digest: object.content_digest(),
        };
        self.lock().insert(key, cached.clone());

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

    /// Fetches `object` and counts the calls in `calls`.
    async fn fetch_counted<K>(
        calls: &Cell<usize>,
        object: Option<K>,
    ) -> Result<Option<K>, Infallible> {
        calls.set(calls.get() + 1);
        Ok(object)
    }

    #[test]
    fn digest_is_stable_across_releases() {
        // Changing the digest function rolls every StatefulSet using data-hash
        let binary_config_map = ConfigMap {
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..ConfigMap::default()
        };
        let secret = Secret {
            data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..Secret::default()
        };

        assert_eq!(
            config_map(&[("foo", "bar")]).content_digest(),
            "401bdc692ecd7b9bb6b02f186e2976c3"
        );
        assert_eq!(
            binary_config_map.content_digest(),
            "20feddae4a413582dab5c1c2698ef5b8"
        );
        assert_eq!(secret.content_digest(), "fa14a91ea83afdfe42d96880fe813616");
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
            config_map(&[("foo", "bar")]).content_digest(),
            changed.content_digest()
        );
    }

    #[test]
    fn digest_distinguishes_ambiguous_entry_splits() {
        // Without length prefixes these would hash identically.
        assert_ne!(
            config_map(&[("ab", "c")]).content_digest(),
            config_map(&[("a", "bc")]).content_digest()
        );
    }

    #[test]
    fn digest_distinguishes_data_from_binary_data() {
        let binary = ConfigMap {
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..ConfigMap::default()
        };

        assert_ne!(
            config_map(&[("foo", "bar")]).content_digest(),
            binary.content_digest()
        );
    }

    #[tokio::test]
    async fn cache_fetches_once_per_resource_version() {
        let cache = DigestCache::default();
        let calls = Cell::new(0);
        let object = stored("uid-1", "100", &[("foo", "bar")]);

        for _ in 0..3 {
            let cached = cache
                .get_or_fetch(&metadata("uid-1", "100"), |_, _| {
                    fetch_counted(&calls, Some(object.clone()))
                })
                .await
                .expect("fetching is infallible")
                .expect("the object exists");
            assert_eq!(cached.digest, object.content_digest());
        }

        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn cache_refetches_when_resource_version_or_uid_changes() {
        let cache = DigestCache::default();
        let calls = Cell::new(0);

        for (uid, resource_version, content) in [
            ("uid-1", "100", "bar"),
            ("uid-1", "101", "baz"),
            ("uid-2", "101", "qux"),
        ] {
            let object = stored(uid, resource_version, &[("foo", content)]);
            let cached = cache
                .get_or_fetch(&metadata(uid, resource_version), |_, _| {
                    fetch_counted(&calls, Some(object.clone()))
                })
                .await
                .expect("fetching is infallible")
                .expect("the object exists");

            assert_eq!(
                cached,
                CachedDigest {
                    uid: uid.to_owned(),
                    resource_version: resource_version.to_owned(),
                    digest: object.content_digest(),
                }
            );
        }

        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn cache_forgets_objects_that_are_gone() {
        let cache = DigestCache::default();
        let calls = Cell::new(0);
        let object = stored("uid-1", "100", &[("foo", "bar")]);
        let existing = metadata("uid-1", "100");
        let fetch_existing = |_, _| fetch_counted(&calls, Some(object.clone()));

        cache
            .get_or_fetch(&existing, fetch_existing)
            .await
            .expect("fetching is infallible");
        let missing = cache
            .get_or_fetch(&metadata("uid-1", "101"), |_, _| {
                fetch_counted::<ConfigMap>(&calls, None)
            })
            .await
            .expect("fetching is infallible");

        assert_eq!(missing, None);

        cache
            .get_or_fetch(&existing, fetch_existing)
            .await
            .expect("fetching is infallible");
        assert_eq!(calls.get(), 3);
    }
}
