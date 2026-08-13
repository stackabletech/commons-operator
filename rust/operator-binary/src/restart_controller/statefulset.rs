use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    marker::PhantomData,
    sync::Arc,
    time::Duration,
};

use futures::{Stream, StreamExt, TryStream, TryStreamExt, stream};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    client::Client,
    k8s_openapi::{
        api::{
            apps::v1::StatefulSet,
            core::v1::{ConfigMap, EnvFromSource, EnvVar, PodSpec, Secret, Volume},
        },
        apimachinery::pkg::apis::meta::v1::ObjectMeta,
    },
    kube::{
        self, Resource, ResourceExt,
        api::{Patch, PatchParams},
        core::{DeserializeGuard, DynamicObject, error_boundary},
        runtime::{
            Config, WatchStreamExt, applier,
            controller::{Action, ReconcileRequest, trigger_self, trigger_with},
            events::{Recorder, Reporter},
            reflector,
            reflector::{Lookup, ObjectRef, Store},
            watcher::{self, watcher},
        },
    },
    logging::controller::{ReconcilerError, report_controller_reconciled},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

use crate::utils::delayed_init::{DelayedInit, InitDropped, Initializer};

const FULL_CONTROLLER_NAME: &str = "statefulset.restarter.commons.stackable.tech";

/// A watched ConfigMap or Secret, reduced to its identity and a digest of its content.
#[derive(Clone, Debug)]
pub struct ContentDigest<K> {
    name: Option<String>,
    namespace: Option<String>,
    uid: Option<String>,
    resource_version: Option<String>,
    digest: String,
    _phantom: PhantomData<K>,
}

impl<K> ContentDigest<K> {
    fn new(metadata: ObjectMeta, digest: String) -> Self {
        Self {
            name: metadata.name,
            namespace: metadata.namespace,
            uid: metadata.uid,
            resource_version: metadata.resource_version,
            digest,
            _phantom: PhantomData,
        }
    }
}

/// Lets a [`Store`] cache these objects, keyed by name and namespace - so a
/// `ContentDigest<ConfigMap>` is found under the same key as the ConfigMap it was made from.
impl<K: Resource> Lookup for ContentDigest<K> {
    type DynamicType = K::DynamicType;

    fn kind(dt: &Self::DynamicType) -> Cow<'_, str> {
        K::kind(dt)
    }

    fn group(dt: &Self::DynamicType) -> Cow<'_, str> {
        K::group(dt)
    }

    fn version(dt: &Self::DynamicType) -> Cow<'_, str> {
        K::version(dt)
    }

    fn plural(dt: &Self::DynamicType) -> Cow<'_, str> {
        K::plural(dt)
    }

    fn name(&self) -> Option<Cow<'_, str>> {
        self.name.as_deref().map(Cow::Borrowed)
    }

    fn namespace(&self) -> Option<Cow<'_, str>> {
        self.namespace.as_deref().map(Cow::Borrowed)
    }

    fn resource_version(&self) -> Option<Cow<'_, str>> {
        self.resource_version.as_deref().map(Cow::Borrowed)
    }

    fn uid(&self) -> Option<Cow<'_, str>> {
        self.uid.as_deref().map(Cow::Borrowed)
    }
}

pub struct Ctx {
    client: Client,
    cms: DelayedInit<Store<ContentDigest<ConfigMap>>>,
    secrets: DelayedInit<Store<ContentDigest<Secret>>>,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
pub enum Error {
    #[snafu(display("StatefulSet object is invalid"))]
    InvalidStatefulSet {
        source: error_boundary::InvalidObject,
    },

    #[snafu(display("failed to patch object {obj_ref}"))]
    PatchFailed {
        source: kube::Error,
        obj_ref: Box<ObjectRef<DynamicObject>>,
    },

    #[snafu(display("configmap initializer was cancelled"))]
    ConfigMapsUninitialized { source: InitDropped },

    #[snafu(display("secrets initializer was cancelled"))]
    SecretsUninitialized { source: InitDropped },
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<DynamicObject>> {
        match self {
            Error::InvalidStatefulSet { .. } => None,
            Error::PatchFailed { obj_ref, .. } => Some(*obj_ref.clone()),
            Error::ConfigMapsUninitialized { .. } => None,
            Error::SecretsUninitialized { .. } => None,
        }
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

/// Digest over the parts of a ConfigMap that a Pod can read.
fn config_map_content_digest(config_map: &ConfigMap) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"configmap/data");
    for (key, value) in config_map.data.iter().flatten() {
        update_entry(&mut hasher, key, value.as_bytes());
    }
    hasher.update(b"configmap/binaryData");
    for (key, value) in config_map.binary_data.iter().flatten() {
        update_entry(&mut hasher, key, &value.0);
    }
    finish_digest(hasher)
}

/// Digest over the parts of a Secret that a Pod can read.
fn secret_content_digest(secret: &Secret) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"secret/data");
    for (key, value) in secret.data.iter().flatten() {
        update_entry(&mut hasher, key, &value.0);
    }
    finish_digest(hasher)
}

/// Reduces a ConfigMap to what the restarter needs, dropping its content.
fn digest_config_map(config_map: ConfigMap) -> ContentDigest<ConfigMap> {
    let digest = config_map_content_digest(&config_map);
    ContentDigest::new(config_map.metadata, digest)
}

/// Reduces a Secret to what the restarter needs, dropping its content.
fn digest_secret(secret: Secret) -> ContentDigest<Secret> {
    let digest = secret_content_digest(&secret);
    ContentDigest::new(secret.metadata, digest)
}

/// Maps the object contained in a watcher event, changing its type.
fn map_event<K, L>(event: watcher::Event<K>, f: impl FnOnce(K) -> L) -> watcher::Event<L> {
    match event {
        watcher::Event::Apply(obj) => watcher::Event::Apply(f(obj)),
        watcher::Event::Delete(obj) => watcher::Event::Delete(f(obj)),
        watcher::Event::InitApply(obj) => watcher::Event::InitApply(f(obj)),
        watcher::Event::Init => watcher::Event::Init,
        watcher::Event::InitDone => watcher::Event::InitDone,
    }
}

#[allow(clippy::type_complexity)]
pub fn create_context(
    client: Client,
) -> (
    Arc<Ctx>,
    Initializer<Store<ContentDigest<ConfigMap>>>,
    Initializer<Store<ContentDigest<Secret>>>,
) {
    let (cm_store_tx, cm_store_delayed) = DelayedInit::new();
    let (secret_store_tx, secret_store_delayed) = DelayedInit::new();
    let ctx = Arc::new(Ctx {
        client,
        cms: cm_store_delayed,
        secrets: secret_store_delayed,
    });

    (ctx, cm_store_tx, secret_store_tx)
}

pub async fn start<F>(
    ctx: Arc<Ctx>,
    cm_store_tx: Initializer<Store<ContentDigest<ConfigMap>>>,
    secret_store_tx: Initializer<Store<ContentDigest<Secret>>>,
    watch_namespace: &WatchNamespace,
    shutdown_signal: F,
) where
    F: Future<Output = ()>,
{
    let stses = watch_namespace.get_api::<DeserializeGuard<StatefulSet>>(&ctx.client);
    let cms = watch_namespace.get_api::<ConfigMap>(&ctx.client);
    let secrets = watch_namespace.get_api::<Secret>(&ctx.client);
    let sts_store = reflector::store::Writer::<DeserializeGuard<StatefulSet>>::new(());
    let cm_store = reflector::store::Writer::<ContentDigest<ConfigMap>>::new(());
    let secret_store = reflector::store::Writer::<ContentDigest<Secret>>::new(());
    let mut cm_store_tx = Some(cm_store_tx);
    let mut secret_store_tx = Some(secret_store_tx);
    let ctx2 = ctx.clone();
    let event_recorder = Arc::new(Recorder::new(
        ctx.client.as_kube_client(),
        Reporter {
            controller: FULL_CONTROLLER_NAME.to_string(),
            instance: None,
        },
    ));

    applier(
        |sts, ctx| Box::pin(reconcile(sts, ctx)),
        error_policy,
        ctx2,
        sts_store.as_reader(),
        stream::select(
            stream::select(
                trigger_all(
                    {
                        let cm_reader = cm_store.as_reader();
                        reflector(
                            cm_store,
                            watcher(
                                cms,
                                watcher::Config::default()
                                    .labels("restarter.stackable.tech/ignore != true"),
                            )
                            .map_ok(|event| map_event(event, digest_config_map)),
                        )
                        .inspect(move |_| {
                            if let Some(tx) = cm_store_tx.take() {
                                tx.init(cm_reader.clone());
                            }
                        })
                        .touched_objects()
                    },
                    sts_store.as_reader(),
                ),
                trigger_all(
                    {
                        let secret_reader = secret_store.as_reader();
                        reflector(
                            secret_store,
                            watcher(
                                secrets,
                                watcher::Config::default()
                                    .labels("restarter.stackable.tech/ignore != true"),
                            )
                            .map_ok(|event| map_event(event, digest_secret)),
                        )
                        .inspect(move |_| {
                            if let Some(tx) = secret_store_tx.take() {
                                tx.init(secret_reader.clone());
                            }
                        })
                        .touched_objects()
                    },
                    sts_store.as_reader(),
                ),
            ),
            trigger_self(
                reflector(
                    sts_store,
                    watcher(
                        stses,
                        watcher::Config::default().labels("restarter.stackable.tech/enabled=true"),
                    ),
                )
                .applied_objects(),
                (),
            ),
        )
        // This uses the same mechanism as kube's Controller does under the hood, see
        // https://github.com/kube-rs/kube/blob/8bcdcb52e1e13c1c1ec59f6118fbed575ac10a4b/kube-runtime/src/controller/mod.rs#L1671
        .take_until(shutdown_signal),
        Config::default(),
    )
    // We can let the reporting happen in the background
    .for_each_concurrent(
        16, // concurrency limit
        |result| {
            // The event_recorder needs to be shared across all invocations, so that
            // events are correctly aggregated
            let event_recorder = event_recorder.clone();
            async move {
                report_controller_reconciled(&event_recorder, FULL_CONTROLLER_NAME, &result).await;
            }
        },
    )
    .await;
}

fn trigger_all<S, K>(
    stream: S,
    store: Store<K>,
) -> impl Stream<Item = Result<ReconcileRequest<K>, S::Error>>
where
    S: TryStream,
    K: Resource<DynamicType = ()> + Clone,
{
    trigger_with(stream, move |_| {
        store
            .state()
            .into_iter()
            .map(|obj| ObjectRef::from_obj(obj.as_ref()))
    })
}

fn find_pod_refs<'a, K: Lookup + 'a>(
    pod_spec: &'a PodSpec,
    volume_ref: impl Fn(&Volume) -> Option<ObjectRef<K>> + 'a,
    env_var_ref: impl Fn(&EnvVar) -> Option<ObjectRef<K>> + 'a,
    env_from_ref: impl Fn(&EnvFromSource) -> Option<ObjectRef<K>> + 'a,
) -> impl Iterator<Item = ObjectRef<K>> + 'a {
    let volume_refs = pod_spec.volumes.iter().flatten().flat_map(volume_ref);
    let pod_containers = pod_spec
        .containers
        .iter()
        .chain(pod_spec.init_containers.iter().flatten());
    let container_env_var_refs = pod_containers
        .clone()
        .flat_map(|container| &container.env)
        .flatten()
        .flat_map(env_var_ref);
    let container_env_from_refs = pod_containers
        .flat_map(|container| &container.env_from)
        .flatten()
        .flat_map(env_from_ref);
    volume_refs
        .chain(container_env_var_refs)
        .chain(container_env_from_refs)
}

pub async fn get_updated_restarter_annotations(
    sts: &StatefulSet,
    ctx: Arc<Ctx>,
) -> Result<BTreeMap<String, String>, Error> {
    let ns = sts.metadata.namespace.as_deref().expect(
        "A StatefulSet observed by a reflector (so send by Kubernetes) always has a namespace set",
    );

    let mut annotations = BTreeMap::<String, String>::new();
    let pod_specs = sts
        .spec
        .iter()
        .flat_map(|sts_spec| sts_spec.template.spec.as_ref());

    let cm_refs = pod_specs
        .clone()
        .flat_map(|pod_spec| {
            find_pod_refs(
                pod_spec,
                |volume| {
                    Some(ObjectRef::<ContentDigest<ConfigMap>>::new(
                        &volume.config_map.as_ref()?.name,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<ContentDigest<ConfigMap>>::new(
                        &env_var
                            .value_from
                            .as_ref()?
                            .config_map_key_ref
                            .as_ref()?
                            .name,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<ContentDigest<ConfigMap>>::new(
                        &env_from.config_map_ref.as_ref()?.name,
                    ))
                },
            )
        })
        .map(|cm_ref| cm_ref.within(ns));
    let cms = ctx.cms.get().await.context(ConfigMapsUninitializedSnafu)?;
    let ignored_cms = sts
        .metadata
        .annotations
        .iter()
        .flatten()
        .filter_map(|(key, value)| {
            key.starts_with("restarter.stackable.tech/ignore-configmap.")
                .then_some(value)
        })
        .collect::<BTreeSet<_>>();
    annotations.extend(
        cm_refs
            .map(|cm_ref| (cm_ref.name.clone(), cms.get(&cm_ref)))
            .map(|(cm_name, cm)| {
                (
                    format!("configmap.restarter.stackable.tech/{cm_name}",),
                    if let Some(cm) = cm
                        && let Some(uid) = &cm.uid
                        && !ignored_cms.contains(&cm_name)
                    {
                        let digest = &cm.digest;
                        format!("{uid}/{digest}")
                    } else {
                        "changes-ignored".to_owned()
                    },
                )
            }),
    );

    let secret_refs = pod_specs
        .flat_map(|pod_spec| {
            find_pod_refs(
                pod_spec,
                |volume| {
                    Some(ObjectRef::<ContentDigest<Secret>>::new(
                        volume.secret.as_ref()?.secret_name.as_deref()?,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<ContentDigest<Secret>>::new(
                        &env_var.value_from.as_ref()?.secret_key_ref.as_ref()?.name,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<ContentDigest<Secret>>::new(
                        &env_from.secret_ref.as_ref()?.name,
                    ))
                },
            )
        })
        .map(|secret_ref| secret_ref.within(ns));
    let secrets = ctx.secrets.get().await.context(SecretsUninitializedSnafu)?;
    let ignored_secrets = sts
        .metadata
        .annotations
        .iter()
        .flatten()
        .filter(|annotation| {
            annotation
                .0
                .starts_with("restarter.stackable.tech/ignore-secret.")
        })
        .map(|x| x.1)
        .collect::<BTreeSet<_>>();
    annotations.extend(
        secret_refs
            .map(|secret_ref| (secret_ref.name.clone(), secrets.get(&secret_ref)))
            .map(|(secret_name, secret)| {
                (
                    format!("secret.restarter.stackable.tech/{secret_name}",),
                    if let Some(secret) = secret
                        && let Some(uid) = &secret.uid
                        && !ignored_secrets.contains(&secret_name)
                    {
                        let digest = &secret.digest;
                        format!("{uid}/{digest}")
                    } else {
                        "changes-ignored".to_owned()
                    },
                )
            }),
    );

    Ok(annotations)
}

async fn reconcile(
    sts: Arc<DeserializeGuard<StatefulSet>>,
    ctx: Arc<Ctx>,
) -> Result<Action, Error> {
    tracing::info!("Starting reconcile");
    let sts = sts
        .0
        .as_ref()
        .map_err(error_boundary::InvalidObject::clone)
        .context(InvalidStatefulSetSnafu)?;
    let ns = sts.metadata.namespace.as_deref().unwrap();

    let stses = kube::Api::<StatefulSet>::namespaced(ctx.client.as_kube_client(), ns);
    stses
        .patch(
            &sts.name_unchecked(),
            &PatchParams {
                force: true,
                field_manager: Some("restarter.stackable.tech/statefulset".to_string()),
                ..PatchParams::default()
            },
            &Patch::Apply(
                // Can't use typed API, see https://github.com/Arnavion/k8s-openapi/issues/112
                json!({
                    "apiVersion": "apps/v1",
                    "kind": "StatefulSet",
                    "metadata": {
                        "name": sts.metadata.name,
                        "namespace": sts.metadata.namespace,
                        "uid": sts.metadata.uid,
                    },
                    "spec": {
                        "template": {
                            "metadata": {
                                "annotations": get_updated_restarter_annotations(sts, ctx).await?,
                            },
                        },
                    },
                }),
            ),
        )
        .await
        .context(PatchFailedSnafu {
            obj_ref: ObjectRef::from_obj(sts).erase(),
        })?;
    Ok(Action::await_change())
}

fn error_policy(_obj: Arc<DeserializeGuard<StatefulSet>>, error: &Error, _ctx: Arc<Ctx>) -> Action {
    match error {
        // root object is invalid, will be requeued when modified anyway
        Error::InvalidStatefulSet { .. } => Action::await_change(),

        _ => Action::requeue(Duration::from_secs(5)),
    }
}

#[cfg(test)]
mod tests {
    use stackable_operator::k8s_openapi::{
        ByteString,
        apimachinery::pkg::apis::meta::v1::{ManagedFieldsEntry, ObjectMeta},
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

    #[test]
    fn digest_is_stable_across_releases() {
        // Changing the digest function rolls every StatefulSet labelled
        // restarter.stackable.tech/enabled=true which we want to avoid
        assert_eq!(
            config_map_content_digest(&config_map(&[("foo", "bar")])),
            "401bdc692ecd7b9bb6b02f186e2976c3"
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
            config_map_content_digest(&config_map(&[("foo", "bar")])),
            config_map_content_digest(&changed)
        );
    }

    #[test]
    fn digest_changes_when_content_changes() {
        assert_ne!(
            config_map_content_digest(&config_map(&[("foo", "bar")])),
            config_map_content_digest(&config_map(&[("foo", "baz")]))
        );
    }

    #[test]
    fn digest_distinguishes_ambiguous_entry_splits() {
        // Without length prefixes these would hash identically.
        assert_ne!(
            config_map_content_digest(&config_map(&[("ab", "c")])),
            config_map_content_digest(&config_map(&[("a", "bc")]))
        );
    }

    #[test]
    fn digest_distinguishes_data_from_binary_data() {
        let binary = ConfigMap {
            binary_data: Some([("foo".to_owned(), ByteString(b"bar".to_vec()))].into()),
            ..ConfigMap::default()
        };

        assert_ne!(
            config_map_content_digest(&config_map(&[("foo", "bar")])),
            config_map_content_digest(&binary)
        );
    }

    fn identifying_metadata() -> ObjectMeta {
        ObjectMeta {
            name: Some("my-config".to_owned()),
            namespace: Some("my-namespace".to_owned()),
            uid: Some("f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c".to_owned()),
            managed_fields: Some(vec![ManagedFieldsEntry {
                manager: Some("third-party-controller".to_owned()),
                ..ManagedFieldsEntry::default()
            }]),
            annotations: Some(
                [(
                    "kubectl.kubernetes.io/last-applied-configuration".to_owned(),
                    r#"{"data":{"password":"aHVudGVyMg=="}}"#.to_owned(),
                )]
                .into(),
            ),
            ..ObjectMeta::default()
        }
    }

    #[test]
    fn digesting_keeps_the_identity() {
        let config_map = ConfigMap {
            metadata: identifying_metadata(),
            ..config_map(&[("foo", "bar")])
        };
        let digest = config_map_content_digest(&config_map);
        let digested = digest_config_map(config_map);

        assert_eq!(digested.digest, digest);
        assert_eq!(digested.name.as_deref(), Some("my-config"));
        assert_eq!(digested.namespace.as_deref(), Some("my-namespace"));
        assert_eq!(
            digested.uid.as_deref(),
            Some("f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c")
        );
    }

    #[test]
    fn digesting_a_secret_retains_no_content() {
        let secret = Secret {
            metadata: identifying_metadata(),
            data: Some([("password".to_owned(), ByteString(b"hunter2".to_vec()))].into()),
            ..Secret::default()
        };
        let digest = secret_content_digest(&secret);
        let digested = digest_secret(secret);

        assert_eq!(digested.digest, digest);
        assert_eq!(digested.name.as_deref(), Some("my-config"));

        let debug = format!("{digested:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(!debug.contains("aHVudGVyMg=="), "{debug}");
    }

    #[test]
    fn store_keys_match_the_refs_built_from_a_pod_template() {
        let digested = digest_config_map(ConfigMap {
            metadata: identifying_metadata(),
            ..config_map(&[("foo", "bar")])
        });

        assert_eq!(
            ObjectRef::from_obj(&digested),
            ObjectRef::<ContentDigest<ConfigMap>>::new("my-config").within("my-namespace")
        );
    }
}
