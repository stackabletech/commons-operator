//! Digests over the content of ConfigMaps and Secrets, which the StatefulSet restarter tracks to
//! detect changes.
//!
//! The restarter watches ConfigMaps and Secrets and replaces their content by its digest before
//! storing them (see [`replace_content_by_digest`]). It also drops all metadata it does not need,
//! as metadata can hold the content too, e.g. the `kubectl.kubernetes.io/last-applied-configuration`
//! annotation of objects created with `kubectl apply`. So its stores hold no content.
//!
//! The digests are keyed with a [`DigestKey`] and bound to the uid of their object, so that anyone
//! who can read them in the Pod template annotations learns nothing about the content beyond
//! whether it changed.

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

/// Annotation holding the content digest of a ConfigMap or Secret in the restarter's stores.
///
/// Only set in memory by [`replace_content_by_digest`], which overwrites any value the object
/// already has. It is never written to the API server.
const CONTENT_DIGEST_ANNOTATION: &str = "restarter.stackable.tech/content-digest";

/// An object whose content can be digested.
pub trait ContentDigest: Resource<DynamicType = ()> {
    /// Keyed digest over the parts of the object that a Pod can read, bound to the object's `uid`.
    fn content_digest(&self, uid: &str, digest_key: &DigestKey) -> String;

    /// Removes the parts of the object that [`ContentDigest::content_digest`] covers.
    fn clear_content(&mut self);
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

    fn clear_content(&mut self) {
        self.data = None;
        self.binary_data = None;
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

    fn clear_content(&mut self) {
        self.data = None;
        self.string_data = None;
    }
}

/// Replaces the content of `object` by its digest, stored in the [`CONTENT_DIGEST_ANNOTATION`], so
/// that no content stays in memory.
///
/// Of the metadata, only the name, namespace, uid and resourceVersion are kept, which is all the
/// restarter needs. All labels, managed fields and other annotations are dropped, as they can
/// hold the content as well.
///
/// Without a uid, which every object from the API server has, no digest is stored.
pub fn replace_content_by_digest<K: ContentDigest>(object: &mut K, digest_key: &DigestKey) {
    let digest = object
        .meta()
        .uid
        .as_deref()
        .map(|uid| object.content_digest(uid, digest_key));
    object.clear_content();
    let metadata = std::mem::take(object.meta_mut());
    *object.meta_mut() = ObjectMeta {
        name: metadata.name,
        namespace: metadata.namespace,
        uid: metadata.uid,
        resource_version: metadata.resource_version,
        annotations: digest.map(|digest| [(CONTENT_DIGEST_ANNOTATION.to_owned(), digest)].into()),
        ..ObjectMeta::default()
    };
}

/// Returns the digest that [`replace_content_by_digest`] stored in `metadata`, if any.
pub fn stored_digest(metadata: &ObjectMeta) -> Option<&str> {
    metadata
        .annotations
        .as_ref()?
        .get(CONTENT_DIGEST_ANNOTATION)
        .map(String::as_str)
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

#[cfg(test)]
mod tests {
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

    /// Changing the digest function rolling-restarts every StatefulSet managed by the restarter.
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

    #[test]
    fn content_is_replaced_by_its_digest() {
        let annotations = [
            (CONTENT_DIGEST_ANNOTATION.to_owned(), "spoofed".to_owned()),
            (
                "kubectl.kubernetes.io/last-applied-configuration".to_owned(),
                r#"{"data":{"foo":"bar"}}"#.to_owned(),
            ),
        ];
        let mut with_uid = ConfigMap {
            metadata: ObjectMeta {
                name: Some("config".to_owned()),
                namespace: Some("default".to_owned()),
                uid: Some(TEST_UID.to_owned()),
                resource_version: Some("12345".to_owned()),
                labels: Some([("foo".to_owned(), "bar".to_owned())].into()),
                annotations: Some(annotations.clone().into()),
                managed_fields: Some(vec![ManagedFieldsEntry::default()]),
                ..ObjectMeta::default()
            },
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..config_map(&[("foo", "bar")])
        };
        let mut without_uid = ConfigMap {
            metadata: ObjectMeta {
                annotations: Some(annotations.into()),
                ..ObjectMeta::default()
            },
            ..config_map(&[("foo", "bar")])
        };
        let expected_digest = with_uid.content_digest(TEST_UID, &TEST_KEY);

        replace_content_by_digest(&mut with_uid, &TEST_KEY);
        replace_content_by_digest(&mut without_uid, &TEST_KEY);

        assert_eq!(
            with_uid,
            ConfigMap {
                metadata: ObjectMeta {
                    name: Some("config".to_owned()),
                    namespace: Some("default".to_owned()),
                    uid: Some(TEST_UID.to_owned()),
                    resource_version: Some("12345".to_owned()),
                    annotations: Some(
                        [(CONTENT_DIGEST_ANNOTATION.to_owned(), expected_digest)].into()
                    ),
                    ..ObjectMeta::default()
                },
                ..ConfigMap::default()
            }
        );
        assert_eq!(without_uid, ConfigMap::default());
    }
}
