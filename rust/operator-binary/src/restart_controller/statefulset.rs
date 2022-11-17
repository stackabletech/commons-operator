use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use futures::{stream, Stream, StreamExt, TryStream};
use serde_json::json;
use snafu::{ResultExt, Snafu};
use stackable_operator::client::Client;
use stackable_operator::k8s_openapi::api::apps::v1::StatefulSet;
use stackable_operator::k8s_openapi::api::core::v1::{
    ConfigMap, EnvFromSource, EnvVar, PodSpec, Secret, Volume,
};
use stackable_operator::kube;
use stackable_operator::kube::api::{ListParams, Patch, PatchParams};
use stackable_operator::kube::core::DynamicObject;
use stackable_operator::kube::runtime::controller::{
    trigger_self, trigger_with, Action, ReconcileRequest,
};
use stackable_operator::kube::runtime::reflector::{ObjectRef, Store};
use stackable_operator::kube::runtime::{applier, reflector, watcher, WatchStreamExt};
use stackable_operator::kube::{Resource, ResourceExt};
use stackable_operator::logging::controller::{report_controller_reconciled, ReconcilerError};
use strum::{EnumDiscriminants, IntoStaticStr};

struct Ctx {
    kube: kube::Client,
    cms: Store<ConfigMap>,
    cms_inited: Arc<AtomicBool>,
    secrets: Store<Secret>,
    secrets_inited: Arc<AtomicBool>,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
enum Error {
    #[snafu(display("failed to patch object {}", obj_ref))]
    PatchFailed {
        source: kube::Error,
        obj_ref: Box<ObjectRef<DynamicObject>>,
    },
    #[snafu(display("configmaps were not yet loaded"))]
    ConfigMapsUninitialized,
    #[snafu(display("secrets were not yet loaded"))]
    SecretsUninitialized,
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<DynamicObject>> {
        match self {
            Error::PatchFailed { obj_ref, .. } => Some(*obj_ref.clone()),
            Error::ConfigMapsUninitialized => None,
            Error::SecretsUninitialized => None,
        }
    }
}

pub async fn start(client: &Client) {
    let kube = client.as_kube_client();
    let stses = kube::Api::<StatefulSet>::all(kube.clone());
    let cms = kube::Api::<ConfigMap>::all(kube.clone());
    let secrets = kube::Api::<Secret>::all(kube.clone());
    let sts_store = reflector::store::Writer::new(());
    let cm_store = reflector::store::Writer::new(());
    let secret_store = reflector::store::Writer::new(());
    let cms_inited = Arc::new(AtomicBool::from(false));
    let secrets_inited = Arc::new(AtomicBool::from(false));

    applier(
        |sts, ctx| Box::pin(reconcile(sts, ctx)),
        error_policy,
        Arc::new(Ctx {
            kube,
            cms: cm_store.as_reader(),
            secrets: secret_store.as_reader(),
            cms_inited: cms_inited.clone(),
            secrets_inited: secrets_inited.clone(),
        }),
        sts_store.as_reader(),
        stream::select(
            stream::select(
                trigger_all(
                    reflector(cm_store, watcher(cms, ListParams::default()))
                        .inspect(|_| cms_inited.store(true, std::sync::atomic::Ordering::SeqCst))
                        .applied_objects(),
                    sts_store.as_reader(),
                ),
                trigger_all(
                    reflector(secret_store, watcher(secrets, ListParams::default()))
                        .inspect(|_| {
                            secrets_inited.store(true, std::sync::atomic::Ordering::SeqCst)
                        })
                        .applied_objects(),
                    sts_store.as_reader(),
                ),
            ),
            trigger_self(
                reflector(
                    sts_store,
                    watcher(
                        stses,
                        ListParams::default().labels("restarter.stackable.tech/enabled=true"),
                    ),
                )
                .applied_objects(),
                (),
            ),
        ),
    )
    .for_each(|res| async move {
        report_controller_reconciled(client, "statefulset.restarter.commons.stackable.tech", &res)
    })
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

async fn reconcile(sts: Arc<StatefulSet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    if !ctx.cms_inited.load(std::sync::atomic::Ordering::SeqCst) {
        return ConfigMapsUninitializedSnafu.fail();
    }
    if !ctx.secrets_inited.load(std::sync::atomic::Ordering::SeqCst) {
        return SecretsUninitializedSnafu.fail();
    }

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
                    Some(ObjectRef::<ConfigMap>::new(
                        volume.config_map.as_ref()?.name.as_deref()?,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<ConfigMap>::new(
                        env_var
                            .value_from
                            .as_ref()?
                            .config_map_key_ref
                            .as_ref()?
                            .name
                            .as_deref()?,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<ConfigMap>::new(
                        env_from.config_map_ref.as_ref()?.name.as_deref()?,
                    ))
                },
            )
        })
        .map(|cm_ref| cm_ref.within(ns));
    annotations.extend(
        cm_refs
            .flat_map(|cm_ref| ctx.cms.get(&cm_ref))
            .flat_map(|cm| {
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
            }),
    );
    let secret_refs = pod_specs
        .flat_map(|pod_spec| {
            find_pod_refs(
                pod_spec,
                |volume| {
                    Some(ObjectRef::<Secret>::new(
                        volume.secret.as_ref()?.secret_name.as_deref()?,
                    ))
                },
                |env_var| {
                    Some(ObjectRef::<Secret>::new(
                        env_var
                            .value_from
                            .as_ref()?
                            .secret_key_ref
                            .as_ref()?
                            .name
                            .as_deref()?,
                    ))
                },
                |env_from| {
                    Some(ObjectRef::<Secret>::new(
                        env_from.secret_ref.as_ref()?.name.as_deref()?,
                    ))
                },
            )
        })
        .map(|secret_ref| secret_ref.within(ns));
    annotations.extend(
        secret_refs
            .flat_map(|secret_ref| ctx.secrets.get(&secret_ref))
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
    let stses = kube::Api::<StatefulSet>::namespaced(ctx.kube.clone(), ns);
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
                                "annotations": annotations,
                            },
                        },
                    },
                }),
            ),
        )
        .await
        .context(PatchFailedSnafu {
            obj_ref: ObjectRef::from_obj(sts.as_ref()).erase(),
        })?;
    Ok(Action::await_change())
}

fn error_policy(_obj: Arc<StatefulSet>, _error: &Error, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(Duration::from_secs(5))
}
