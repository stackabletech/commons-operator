use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures::{Stream, StreamExt, TryStream, stream};
use serde_json::json;
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    client::Client,
    k8s_openapi::api::{
        apps::v1::StatefulSet,
        core::v1::{ConfigMap, EnvFromSource, EnvVar, PodSpec, Secret, Volume},
    },
    kube::{
        self, Resource, ResourceExt,
        api::{PartialObjectMeta, Patch, PatchParams},
        core::{DeserializeGuard, DynamicObject, error_boundary},
        runtime::{
            Config, WatchStreamExt, applier,
            controller::{Action, ReconcileRequest, trigger_self, trigger_with},
            events::{Recorder, Reporter},
            metadata_watcher, reflector,
            reflector::{ObjectRef, Store},
            watcher,
        },
    },
    logging::controller::{ReconcilerError, report_controller_reconciled},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

use crate::utils::delayed_init::{DelayedInit, InitDropped, Initializer};

const FULL_CONTROLLER_NAME: &str = "statefulset.restarter.commons.stackable.tech";

pub struct Ctx {
    client: Client,
    cms: DelayedInit<Store<PartialObjectMeta<ConfigMap>>>,
    secrets: DelayedInit<Store<PartialObjectMeta<Secret>>>,
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

pub fn create_context(
    client: Client,
) -> (
    Arc<Ctx>,
    Initializer<Store<PartialObjectMeta<ConfigMap>>>,
    Initializer<Store<PartialObjectMeta<Secret>>>,
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

pub async fn start(
    ctx: Arc<Ctx>,
    cm_store_tx: Initializer<Store<PartialObjectMeta<ConfigMap>>>,
    secret_store_tx: Initializer<Store<PartialObjectMeta<Secret>>>,
    watch_namespace: &WatchNamespace,
) {
    let stses = watch_namespace.get_api::<DeserializeGuard<StatefulSet>>(&ctx.client);
    let cms = watch_namespace.get_api::<ConfigMap>(&ctx.client);
    let secrets = watch_namespace.get_api::<Secret>(&ctx.client);
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
                        reflector(cm_store, metadata_watcher(cms, watcher::Config::default()))
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
                            metadata_watcher(secrets, watcher::Config::default()),
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
        ),
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

pub async fn get_updated_restarter_annotations(
    sts: &StatefulSet,
    ctx: Arc<Ctx>,
) -> Result<BTreeMap<String, String>, Error> {
    let ns = sts.metadata.namespace.as_deref().unwrap();
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
    annotations.extend(cm_refs.flat_map(|cm_ref| cms.get(&cm_ref)).flat_map(|cm| {
        Some((
            format!(
                "configmap.restarter.stackable.tech/{}",
                cm.metadata.name.as_ref()?
            ),
            format!(
                "{}/{}",
                cm.metadata.uid.as_ref()?,
                cm.metadata.resource_version.as_ref()?
            ),
        ))
    }));
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
    annotations.extend(
        secret_refs
            .flat_map(|secret_ref| secrets.get(&secret_ref))
            .flat_map(|cm| {
                Some((
                    format!(
                        "secret.restarter.stackable.tech/{}",
                        cm.metadata.name.as_ref()?
                    ),
                    format!(
                        "{}/{}",
                        cm.metadata.uid.as_ref()?,
                        cm.metadata.resource_version.as_ref()?
                    ),
                ))
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
                                "annotations": get_updated_restarter_annotations(&sts, ctx).await?,
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
