use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    future::Future,
    sync::Arc,
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
use strum::{EnumDiscriminants, IntoStaticStr};

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

pub struct Ctx {
    client: Client,
    cms: DelayedInit<Store<PartialObjectMeta<ConfigMap>>>,
    secrets: DelayedInit<Store<PartialObjectMeta<Secret>>>,
    cm_digests: DigestCache<ConfigMap>,
    secret_digests: DigestCache<Secret>,
    digest_key: DigestKeyStore,
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

/// Returns `previous_value` if it is the `uid/resourceVersion` value that releases before content
/// digests wrote for the object with the given `metadata`, i.e. the object has not been written
/// since the Pods were last rolled with that value.
///
/// Keeping this value lets the operator upgrade from those releases without restarting any Pods.
/// The Pods are restarted on the next write to the object instead, as they would have been by
/// those releases, and from then on the annotation holds the content digest. Objects that are
/// never written keep the value indefinitely, so this has no planned removal: removing it would
/// cost exactly the restart it avoids.
///
/// A digest never matches, as it is 32 hexadecimal characters, while resourceVersions written by
/// etcd are decimal numbers of at most 19 digits.
fn unchanged_resource_version_value(
    metadata: &ObjectMeta,
    previous_value: Option<&str>,
) -> Option<String> {
    let value = format!(
        "{}/{}",
        metadata.uid.as_deref()?,
        metadata.resource_version.as_deref()?
    );
    (Some(value.as_str()) == previous_value).then_some(value)
}

/// Value of the Pod template annotation tracking `object`.
///
/// `previous_value` is the value the Pod template annotation currently has, if any. This fetches
/// the object to digest its content, unless `digests` already holds the digest for its current
/// resourceVersion, or `previous_value` is still up to date, see
/// [`unchanged_resource_version_value`].
///
/// The value is `changes-ignored` if the object is `ignored` via the StatefulSet annotations, or
/// missing from the store (`None`) or the API, i.e. it doesn't exist (yet), e.g. because the
/// StatefulSet was created before it, or it has the label `restarter.stackable.tech/ignore=true`
/// (filtered out by the watcher). This is intended: once the object shows up, the value changes
/// to its digest (unless ignored via the StatefulSet annotations), which restarts the Pods so that
/// they pick it up. Leaving out the annotation instead would not avoid any restart, but cause
/// additional ones, e.g. when an object ignored via a StatefulSet annotation is created after the
/// StatefulSet.
async fn tracking_value<K>(
    ctx: &Ctx,
    digests: &DigestCache<K>,
    object: Option<&PartialObjectMeta<K>>,
    ignored: bool,
    previous_value: Option<&str>,
) -> Result<String, Error>
where
    K: ContentDigest + Resource<Scope = NamespaceResourceScope> + Clone + Debug + DeserializeOwned,
{
    let changes_ignored = || "changes-ignored".to_owned();
    let Some(object) = object.filter(|_| !ignored) else {
        return Ok(changes_ignored());
    };
    let metadata = &object.metadata;
    if let Some(value) = unchanged_resource_version_value(metadata, previous_value) {
        return Ok(value);
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
    Ok(cached.map_or_else(changes_ignored, |cached| {
        format!("{}/{}", cached.uid, cached.digest)
    }))
}

pub async fn get_updated_restarter_annotations(
    sts: &StatefulSet,
    ctx: Arc<Ctx>,
) -> Result<BTreeMap<String, String>, Error> {
    let ns = sts.metadata.namespace.as_deref().expect(
        "A StatefulSet observed by a reflector (so send by Kubernetes) always has a namespace set",
    );
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
        let key = annotation_key(CONFIGMAP_ANNOTATION_PREFIX, &cm_ref.name)?;
        let value = tracking_value(
            &ctx,
            &ctx.cm_digests,
            cms.get(&cm_ref).as_deref(),
            ignored_cms.contains(&cm_ref.name),
            previous_annotations.and_then(|previous| previous.get(&key).map(String::as_str)),
        )
        .await?;
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
        let key = annotation_key(SECRET_ANNOTATION_PREFIX, &secret_ref.name)?;
        let value = tracking_value(
            &ctx,
            &ctx.secret_digests,
            secrets.get(&secret_ref).as_deref(),
            ignored_secrets.contains(&secret_ref.name),
            previous_annotations.and_then(|previous| previous.get(&key).map(String::as_str)),
        )
        .await?;
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
