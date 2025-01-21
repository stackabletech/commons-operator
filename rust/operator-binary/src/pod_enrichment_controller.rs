// Deprecated for removal, see https://github.com/stackabletech/commons-operator/issues/292

use std::{collections::BTreeMap, str::FromStr, sync::Arc, time::Duration};

use futures::StreamExt;
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    k8s_openapi::api::core::v1::{Node, Pod},
    kube::{
        core::{error_boundary, DeserializeGuard, ObjectMeta},
        runtime::{
            controller,
            events::{Recorder, Reporter},
            reflector::ObjectRef,
            watcher, Controller,
        },
        Resource,
    },
    logging::controller::{report_controller_reconciled, ReconcilerError},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

const FULL_CONTROLLER_NAME: &str = "pod.enrichment.commons.stackable.tech";
const FIELD_MANAGER_SCOPE: &str = "enrichment.stackable.tech/pod";
const ANNOTATION_NODE_ADDRESS: &str = "enrichment.stackable.tech/node-address";

struct Ctx {
    client: stackable_operator::client::Client,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
pub enum Error {
    #[snafu(display("Pod object is invalid"))]
    InvalidPod {
        source: error_boundary::InvalidObject,
    },

    #[snafu(display("failed to get {node} for Pod"))]
    GetNode {
        source: stackable_operator::client::Error,
        node: ObjectRef<Node>,
    },

    #[snafu(display("failed to update Pod"))]
    UpdatePod {
        source: stackable_operator::client::Error,
    },
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<stackable_operator::kube::core::DynamicObject>> {
        match self {
            Error::InvalidPod { source: _ } => None,
            Error::GetNode { node, .. } => Some(node.clone().erase()),
            Error::UpdatePod { source: _ } => None,
        }
    }
}

pub async fn start(client: &stackable_operator::client::Client, watch_namespace: &WatchNamespace) {
    let event_recorder = Arc::new(Recorder::new(
        client.as_kube_client(),
        Reporter {
            controller: FULL_CONTROLLER_NAME.to_string(),
            instance: None,
        },
    ));
    let controller = Controller::new(
        watch_namespace.get_api::<DeserializeGuard<Pod>>(client),
        watcher::Config::default().labels("enrichment.stackable.tech/enabled=true"),
    );
    let pods = controller.store();
    controller
        .watches(
            client.get_all_api::<DeserializeGuard<Node>>(),
            watcher::Config::default(),
            move |node| {
                pods.state()
                    .into_iter()
                    .filter(move |pod| {
                        let Ok(pod) = &pod.0 else {
                            return false;
                        };
                        pod.spec.as_ref().and_then(|s| s.node_name.as_deref())
                            == node.meta().name.as_deref()
                    })
                    .map(|pod| ObjectRef::from_obj(&*pod))
            },
        )
        .run(
            reconcile,
            error_policy,
            Arc::new(Ctx {
                client: client.clone(),
            }),
        )
        // We can let the reporting happen in the background
        .for_each_concurrent(
            16, // concurrency limit
            |result| {
                // The event_recorder needs to be shared across all invocations, so that
                // events are correctly aggregated
                let event_recorder = event_recorder.clone();
                async move {
                    report_controller_reconciled(&event_recorder, FULL_CONTROLLER_NAME, &result)
                        .await;
                }
            },
        )
        .await;
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord, strum::EnumString)]
pub enum NodeAddressType {
    ExternalIP,
    InternalIP,
}

async fn reconcile(
    pod: Arc<DeserializeGuard<Pod>>,
    ctx: Arc<Ctx>,
) -> Result<controller::Action, Error> {
    tracing::info!("Starting reconcile");
    let pod = pod
        .0
        .as_ref()
        .map_err(error_boundary::InvalidObject::clone)
        .context(InvalidPodSnafu)?;

    let node_name = pod.spec.as_ref().and_then(|s| s.node_name.as_deref());
    let node = if let Some(node_name) = node_name {
        ctx.client
            .get::<Node>(node_name, &())
            .await
            .with_context(|_| GetNodeSnafu {
                node: ObjectRef::new(node_name),
            })?
    } else {
        // this condition is normal enough during pod setup that we don't want to cause a bunch of
        // error messages...
        tracing::debug!("Pod has not yet been scheduled to a Node");
        return Ok(controller::Action::await_change());
    };

    let mut annotations = BTreeMap::new();

    let node_addr = node
        .status
        .iter()
        .flat_map(|s| &s.addresses)
        .flatten()
        .filter_map(|addr| Some((NodeAddressType::from_str(&addr.type_).ok()?, &addr.address)))
        .min_by_key(|(ty, _)| *ty)
        .map(|(_, addr)| addr);
    if let Some(node_addr) = node_addr {
        annotations.insert(ANNOTATION_NODE_ADDRESS.to_string(), node_addr.clone());
    }

    let patch = Pod {
        metadata: ObjectMeta {
            name: pod.metadata.name.clone(),
            namespace: pod.metadata.namespace.clone(),
            uid: pod.metadata.uid.clone(),
            annotations: Some(annotations),
            ..ObjectMeta::default()
        },
        ..Pod::default()
    };
    ctx.client
        .apply_patch(FIELD_MANAGER_SCOPE, &patch, &patch)
        .await
        .context(UpdatePodSnafu)?;
    Ok(controller::Action::await_change())
}

fn error_policy(
    _obj: Arc<DeserializeGuard<Pod>>,
    error: &Error,
    _ctx: Arc<Ctx>,
) -> controller::Action {
    match error {
        // root object is invalid, will be requeued when modified anyway
        Error::InvalidPod { .. } => controller::Action::await_change(),

        _ => controller::Action::requeue(Duration::from_secs(5)),
    }
}
