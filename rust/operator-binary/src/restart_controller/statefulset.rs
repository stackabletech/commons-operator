use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt::Debug,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use futures::{Stream, StreamExt, TryStream, stream};
use serde::de::DeserializeOwned;
use serde_json::json;
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    client::Client,
    constants::RESTART_CONTROLLER_ENABLED_LABEL,
    k8s_openapi::{
        NamespaceResourceScope,
        api::{
            apps::v1::StatefulSet,
            core::v1::{ConfigMap, EnvFromSource, EnvVar, PodSpec, Secret, Volume},
        },
        apimachinery::pkg::apis::meta::v1::ObjectMeta,
    },
    kube::{
        self, Resource, ResourceExt,
        api::{PartialObjectMeta, Patch, PatchParams},
        core::{DeserializeGuard, DynamicObject, error_boundary},
        runtime::{
            Config, WatchStreamExt, applier,
            controller::{Action, ReconcileRequest, trigger_self, trigger_with},
            events::{Recorder, Reporter},
            reflector::{self, ObjectRef, Store},
            watcher::{self, watcher},
        },
    },
    kvp::{self, KeyError},
    logging::controller::{ReconcilerError, report_controller_reconciled},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, EnumString, IntoStaticStr};

use crate::{
    restart_controller::{
        content_digest::{ContentDigest, DigestCache},
        digest_key::{self, DigestKeyStore},
    },
    utils::delayed_init::{DelayedInit, InitDropped, Initializer},
};

const FULL_CONTROLLER_NAME: &str = "statefulset.restarter.commons.stackable.tech";

/// Prefix of the Pod annotations tracking the referenced ConfigMaps.
const CONFIGMAP_ANNOTATION_PREFIX: &str = "configmap.restarter.stackable.tech";

/// Prefix of the Pod annotations tracking the referenced Secrets.
const SECRET_ANNOTATION_PREFIX: &str = "secret.restarter.stackable.tech";

/// StatefulSet annotation selecting the [`ChangeTrackingMechanism`].
const CHANGE_TRACKING_MECHANISM_ANNOTATION: &str =
    "restarter.stackable.tech/change-tracking-mechanism";

/// How the restarter decides that a referenced ConfigMap or Secret has changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "kebab-case")]
enum ChangeTrackingMechanism {
    /// Every write to the object counts as a change, including metadata-only writes.
    ///
    /// Only needs the metadata of the object, which the restarter watches anyway.
    #[default]
    ResourceVersion,

    /// Only changes to the content (`data` and `binaryData`) count as a change.
    ///
    /// Fetches the object whenever its resourceVersion changes, to digest its content.
    DataHash,
}

impl ChangeTrackingMechanism {
    /// Reads the mechanism from the StatefulSet annotation, falling back to the default.
    fn from_statefulset(sts: &StatefulSet) -> Result<Self, Error> {
        match sts.annotations().get(CHANGE_TRACKING_MECHANISM_ANNOTATION) {
            Some(value) => value
                .parse()
                .context(InvalidChangeTrackingMechanismSnafu { value }),
            None => Ok(Self::default()),
        }
    }
}

pub struct Ctx {
    client: Client,
    cms: DelayedInit<Store<PartialObjectMeta<ConfigMap>>>,
    secrets: DelayedInit<Store<PartialObjectMeta<Secret>>>,
    cm_digests: DigestCache<ConfigMap>,
    secret_digests: DigestCache<Secret>,
    digest_key: DigestKeyStore,

    /// Namespace and name of the StatefulSets already warned about the deprecation of
    /// [`ChangeTrackingMechanism::ResourceVersion`], see [`warn_deprecated_mechanism`].
    deprecation_warned: Mutex<HashSet<(String, String)>>,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
pub enum Error {
    #[snafu(display("StatefulSet object is invalid"))]
    InvalidStatefulSet {
        #[snafu(source(from(error_boundary::InvalidObject, Box::new)))]
        source: Box<error_boundary::InvalidObject>,
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

    #[snafu(display("failed to build annotation key for object {object_name:?}"))]
    InvalidAnnotationKey {
        source: KeyError,
        object_name: String,
    },

    #[snafu(display(
        "invalid value {value:?} for StatefulSet annotation {CHANGE_TRACKING_MECHANISM_ANNOTATION:?}, expected one of \"resource-version\" or \"data-hash\""
    ))]
    InvalidChangeTrackingMechanism {
        source: strum::ParseError,
        value: String,
    },

    #[snafu(display("failed to fetch {obj_ref} to digest its content"))]
    FetchContentFailed {
        source: stackable_operator::client::Error,
        obj_ref: Box<ObjectRef<DynamicObject>>,
    },

    #[snafu(display("failed to load the key for content digests"))]
    LoadDigestKey { source: digest_key::Error },
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
            Error::InvalidAnnotationKey { .. } => None,
            Error::InvalidChangeTrackingMechanism { .. } => None,
            Error::FetchContentFailed { obj_ref, .. } => Some(*obj_ref.clone()),
            Error::LoadDigestKey { .. } => None,
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn create_context(
    client: Client,
    operator_namespace: String,
) -> (
    Arc<Ctx>,
    Initializer<Store<PartialObjectMeta<ConfigMap>>>,
    Initializer<Store<PartialObjectMeta<Secret>>>,
) {
    let (cm_store_tx, cm_store_delayed) = DelayedInit::new();
    let (secret_store_tx, secret_store_delayed) = DelayedInit::new();
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        cms: cm_store_delayed,
        secrets: secret_store_delayed,
        cm_digests: DigestCache::default(),
        secret_digests: DigestCache::default(),
        digest_key: DigestKeyStore::new(client, operator_namespace),
        deprecation_warned: Mutex::default(),
    });

    (ctx, cm_store_tx, secret_store_tx)
}

pub async fn start<F>(
    ctx: Arc<Ctx>,
    cm_store_tx: Initializer<Store<PartialObjectMeta<ConfigMap>>>,
    secret_store_tx: Initializer<Store<PartialObjectMeta<Secret>>>,
    watch_namespace: &WatchNamespace,
    shutdown_signal: F,
) where
    F: Future<Output = ()>,
{
    let stses = watch_namespace.get_api::<DeserializeGuard<StatefulSet>>(&ctx.client);
    let cms = watch_namespace.get_api::<PartialObjectMeta<ConfigMap>>(&ctx.client);
    let secrets = watch_namespace.get_api::<PartialObjectMeta<Secret>>(&ctx.client);
    let sts_store = reflector::store::Writer::<DeserializeGuard<StatefulSet>>::new(());
    let cm_store = reflector::store::Writer::<PartialObjectMeta<ConfigMap>>::new(());
    let secret_store = reflector::store::Writer::<PartialObjectMeta<Secret>>::new(());
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
                        reflector::reflector(
                            cm_store,
                            watcher(
                                cms,
                                watcher::Config::default()
                                    .labels("restarter.stackable.tech/ignore != true"),
                            ),
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
                        reflector::reflector(
                            secret_store,
                            watcher(
                                secrets,
                                watcher::Config::default()
                                    .labels("restarter.stackable.tech/ignore != true"),
                            ),
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
                reflector::reflector(
                    sts_store,
                    watcher(
                        stses,
                        watcher::Config::default()
                            .labels(&RESTART_CONTROLLER_ENABLED_LABEL.to_string()),
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

fn find_pod_refs<'a, K: Resource + 'a>(
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

/// Builds the annotation key tracking the referenced ConfigMap or Secret named `object_name`.
///
/// Kubernetes object names are DNS subdomains and can be up to 253 characters long, but the name
/// part of an annotation key (everything after the `/`) is a qualified name and must not exceed 63
/// characters. Embedding the object name verbatim therefore produced an invalid annotation key for
/// longer names, which made Kubernetes reject the entire StatefulSet - either at admission time via
/// our mutating webhook, or when this controller patched it.
///
/// So instead we rely on [`kvp::Key::new_trimmed`] to keep the name within limits.
fn annotation_key(prefix: &str, object_name: &str) -> Result<String, Error> {
    let key = kvp::Key::new_trimmed(Some(prefix), object_name)
        .context(InvalidAnnotationKeySnafu { object_name })?;

    Ok(key.to_string())
}

/// Value of the Pod template annotation tracking the object with the given `metadata` for
/// `ChangeTrackingMechanism::ResourceVersion`, or `None` if the metadata lacks uid or
/// resourceVersion.
fn resource_version_value(metadata: &ObjectMeta) -> Option<String> {
    let uid = metadata.uid.as_deref()?;
    let resource_version = metadata.resource_version.as_deref()?;
    Some(format!("{uid}/{resource_version}"))
}

/// Returns `previous_value` if it is the [`ChangeTrackingMechanism::ResourceVersion`] value of the
/// object with the given `metadata`, i.e. the object has not been written since the Pods were last
/// rolled with that value.
///
/// Keeping this value with [`ChangeTrackingMechanism::DataHash`] switches a StatefulSet from
/// `resource-version` to `data-hash` without restarting its Pods. The Pods are restarted on the next
/// write to the object instead, as they would have been with `resource-version`, and from then on
/// the annotation holds the content digest.
///
/// A digest never matches, as it is 32 hexadecimal characters, while resourceVersions written by
/// etcd are decimal numbers of at most 19 digits.
fn unchanged_resource_version_value(
    metadata: &ObjectMeta,
    previous_value: Option<&str>,
) -> Option<String> {
    resource_version_value(metadata).filter(|value| Some(value.as_str()) == previous_value)
}

/// Value of the Pod template annotation tracking `object` according to `mechanism`, or `None` if
/// it cannot be determined, e.g. because the object was deleted in the meantime.
///
/// `previous_value` is the value the Pod template annotation currently has, if any.
///
/// For [`ChangeTrackingMechanism::DataHash`] this fetches the object, unless `digests` already
/// holds the digest for its current resourceVersion, or `previous_value` is still up to date, see
/// [`unchanged_resource_version_value`].
async fn tracking_value<K>(
    ctx: &Ctx,
    digests: &DigestCache<K>,
    object: &PartialObjectMeta<K>,
    mechanism: ChangeTrackingMechanism,
    previous_value: Option<&str>,
) -> Result<Option<String>, Error>
where
    K: ContentDigest + Resource<Scope = NamespaceResourceScope> + Clone + Debug + DeserializeOwned,
{
    let metadata = &object.metadata;
    match mechanism {
        ChangeTrackingMechanism::ResourceVersion => Ok(resource_version_value(metadata)),
        ChangeTrackingMechanism::DataHash => {
            if let Some(value) = unchanged_resource_version_value(metadata, previous_value) {
                return Ok(Some(value));
            }
            let digest_key = ctx.digest_key.get().await.context(LoadDigestKeySnafu)?;
            let cached = digests
                .get_or_fetch(metadata, digest_key, |name, namespace| {
                    ctx.client.get_opt::<K>(name, namespace)
                })
                .await
                .with_context(|_| FetchContentFailedSnafu {
                    obj_ref: ObjectRef::from_obj(object).erase(),
                })?;
            Ok(cached.map(|cached| format!("{}/{}", cached.uid, cached.digest)))
        }
    }
}

/// Logs the deprecation of [`ChangeTrackingMechanism::ResourceVersion`] for the StatefulSet
/// `name` in `namespace`, once per StatefulSet and operator run.
///
/// Every write to a watched ConfigMap or Secret reconciles every StatefulSet (see [`trigger_all`]),
/// so warning on every call would repeat the warning for each such write. Keyed by name instead of
/// uid, because the webhook sees StatefulSets on creation before they have a uid.
fn warn_deprecated_mechanism(ctx: &Ctx, namespace: &str, name: String) {
    let first_warning = ctx
        .deprecation_warned
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((namespace.to_owned(), name.clone()));
    if first_warning {
        tracing::warn!(
            k8s.object.namespace = namespace,
            k8s.object.name = name,
            "The change tracking mechanism \"resource-version\" is deprecated and will be removed \
             in the next release, which will use \"data-hash\" for every StatefulSet and ignore \
             the value of the annotation {CHANGE_TRACKING_MECHANISM_ANNOTATION:?}"
        );
    }
}

pub async fn get_updated_restarter_annotations(
    sts: &StatefulSet,
    ctx: Arc<Ctx>,
) -> Result<BTreeMap<String, String>, Error> {
    let ns = sts.metadata.namespace.as_deref().expect(
        "A StatefulSet observed by a reflector (so send by Kubernetes) always has a namespace set",
    );
    let mechanism = ChangeTrackingMechanism::from_statefulset(sts)?;
    if mechanism == ChangeTrackingMechanism::ResourceVersion
        && sts
            .annotations()
            .contains_key(CHANGE_TRACKING_MECHANISM_ANNOTATION)
    {
        warn_deprecated_mechanism(&ctx, ns, sts.name_any());
    }
    let previous_annotations = sts
        .spec
        .as_ref()
        .and_then(|sts_spec| sts_spec.template.metadata.as_ref())
        .and_then(|metadata| metadata.annotations.as_ref());

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
                    Some(ObjectRef::<PartialObjectMeta<ConfigMap>>::new(
                        &volume.config_map.as_ref()?.name,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<PartialObjectMeta<ConfigMap>>::new(
                        &env_var
                            .value_from
                            .as_ref()?
                            .config_map_key_ref
                            .as_ref()?
                            .name,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<PartialObjectMeta<ConfigMap>>::new(
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
    for cm_ref in cm_refs {
        let cm_name = &cm_ref.name;
        let cm = cms.get(&cm_ref);

        // It is intended that "changes-ignored" is also used if the ConfigMap is not in the store,
        // i.e. it doesn't exist (yet), e.g. because the StatefulSet was created before the
        // ConfigMap, or it has the label `restarter.stackable.tech/ignore=true` (filtered out by the
        // watcher). Once the ConfigMap shows up, the value changes to its tracking value (see
        // `ChangeTrackingMechanism`, unless ignored via the StatefulSet annotations), which
        // restarts the Pods so that they pick it up.
        // Leaving out the annotation instead would not avoid any restart, but cause additional
        // ones, e.g. when an object ignored via a StatefulSet annotation is created after the
        // StatefulSet.
        let key = annotation_key(CONFIGMAP_ANNOTATION_PREFIX, cm_name)?;
        let previous_value = previous_annotations
            .and_then(|previous| previous.get(&key))
            .map(String::as_str);
        let value = match cm {
            Some(cm) if !ignored_cms.contains(cm_name) => {
                tracking_value(&ctx, &ctx.cm_digests, &cm, mechanism, previous_value).await?
            }
            _ => None,
        }
        .unwrap_or_else(|| "changes-ignored".to_owned());

        annotations.insert(key, value);
    }

    let secret_refs = pod_specs
        .flat_map(|pod_spec| {
            find_pod_refs(
                pod_spec,
                |volume| {
                    Some(ObjectRef::<PartialObjectMeta<Secret>>::new(
                        volume.secret.as_ref()?.secret_name.as_deref()?,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<PartialObjectMeta<Secret>>::new(
                        &env_var.value_from.as_ref()?.secret_key_ref.as_ref()?.name,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<PartialObjectMeta<Secret>>::new(
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
    for secret_ref in secret_refs {
        let secret_name = &secret_ref.name;
        let secret = secrets.get(&secret_ref);

        // Same as for ConfigMaps above, "changes-ignored" is intended for Secrets not in the store.
        let key = annotation_key(SECRET_ANNOTATION_PREFIX, secret_name)?;
        let previous_value = previous_annotations
            .and_then(|previous| previous.get(&key))
            .map(String::as_str);
        let value = match secret {
            Some(secret) if !ignored_secrets.contains(secret_name) => {
                tracking_value(
                    &ctx,
                    &ctx.secret_digests,
                    &secret,
                    mechanism,
                    previous_value,
                )
                .await?
            }
            _ => None,
        }
        .unwrap_or_else(|| "changes-ignored".to_owned());

        annotations.insert(key, value);
    }

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
        Error::InvalidChangeTrackingMechanism { .. } => Action::await_change(),

        _ => Action::requeue(Duration::from_secs(5)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annotation_names() {
        assert_eq!(
            annotation_key(CONFIGMAP_ANNOTATION_PREFIX, "my-configmap")
                .expect("annotation key for a short ConfigMap name must be valid"),
            "configmap.restarter.stackable.tech/my-configmap"
        );
        assert_eq!(
            annotation_key(
                SECRET_ANNOTATION_PREFIX,
                "secret-not-ignored-with-very-looooooong-name-with-63-characters"
            )
            .expect("annotation key for a Secret name of exactly 63 characters must be valid"),
            "secret.restarter.stackable.tech/secret-not-ignored-with-very-looooooong-name-with-63-characters"
        );
        assert_eq!(
            annotation_key(
                SECRET_ANNOTATION_PREFIX,
                "hiverest-owner-user.stackable-postgres-cluster.credentials.postgresql.acid.zalan.do"
            )
            .expect("annotation key for an overly long Secret name must be shortened and valid"),
            "secret.restarter.stackable.tech/hiverest-owner-user.stackable-postgres-cluster.credent-8221aa71"
        );
    }

    fn statefulset_with_mechanism(mechanism: Option<&str>) -> StatefulSet {
        StatefulSet {
            metadata: ObjectMeta {
                annotations: mechanism.map(|mechanism| {
                    [(
                        CHANGE_TRACKING_MECHANISM_ANNOTATION.to_owned(),
                        mechanism.to_owned(),
                    )]
                    .into()
                }),
                ..ObjectMeta::default()
            },
            ..StatefulSet::default()
        }
    }

    #[test]
    fn change_tracking_mechanism_is_read_from_the_annotation() {
        for (annotation, expected) in [
            (None, Some(ChangeTrackingMechanism::ResourceVersion)),
            (
                Some("resource-version"),
                Some(ChangeTrackingMechanism::ResourceVersion),
            ),
            (Some("data-hash"), Some(ChangeTrackingMechanism::DataHash)),
            (Some("DataHash"), None),
        ] {
            assert_eq!(
                ChangeTrackingMechanism::from_statefulset(&statefulset_with_mechanism(annotation))
                    .ok(),
                expected
            );
        }
    }

    #[test]
    fn resource_version_value_matches_previous_releases() {
        let metadata = ObjectMeta {
            uid: Some("f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c".to_owned()),
            resource_version: Some("12345".to_owned()),
            ..ObjectMeta::default()
        };

        assert_eq!(
            resource_version_value(&metadata).as_deref(),
            Some("f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c/12345")
        );
        assert_eq!(resource_version_value(&ObjectMeta::default()), None);
    }

    #[test]
    fn resource_version_value_is_kept_until_the_object_is_written() {
        let uid = "f9dc0a8f-5f4b-4f52-9d0f-1a0ba1e0bd2c";
        let metadata = ObjectMeta {
            uid: Some(uid.to_owned()),
            resource_version: Some("12345".to_owned()),
            ..ObjectMeta::default()
        };

        for (previous_value, expected) in [
            (Some(format!("{uid}/12345")), Some(format!("{uid}/12345"))),
            (Some(format!("{uid}/12344")), None),
            (
                Some("0a1b2c3d-0000-0000-0000-000000000000/12345".to_owned()),
                None,
            ),
            (
                Some(format!("{uid}/0123456789abcdef0123456789abcdef")),
                None,
            ),
            (Some("changes-ignored".to_owned()), None),
            (None, None),
        ] {
            assert_eq!(
                unchanged_resource_version_value(&metadata, previous_value.as_deref()),
                expected,
                "previous value {previous_value:?}"
            );
        }

        assert_eq!(
            unchanged_resource_version_value(&ObjectMeta::default(), Some(&format!("{uid}/12345"))),
            None
        );
    }
}
