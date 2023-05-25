use std::{collections::BTreeMap, str::FromStr, sync::Arc, time::Duration};

use futures::StreamExt;
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    k8s_openapi::api::core::v1::{Node, Pod},
    kube::{
        core::ObjectMeta,
        runtime::{controller, reflector::ObjectRef, watcher, Controller},
    },
    logging::controller::{report_controller_reconciled, ReconcilerError},
};
use strum::{EnumDiscriminants, IntoStaticStr};

const FIELD_MANAGER_SCOPE: &str = "enrichment.stackable.tech/pod";
const ANNOTATION_NODE_ADDRESS: &str = "enrichment.stackable.tech/node-address";

struct Ctx {
    client: stackable_operator::client::Client,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
pub enum Error {
    #[snafu(display("failed to get {node} for Pod"))]
    GetNode {
        source: stackable_operator::error::Error,
        node: ObjectRef<Node>,
    },
    #[snafu(display("failed to update Pod"))]
    UpdatePod {
        source: stackable_operator::error::Error,
    },
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<stackable_operator::kube::core::DynamicObject>> {
        match self {
            Error::GetNode { node, .. } => Some(node.clone().erase()),
            Error::UpdatePod { source: _ } => None,
        }
    }
}

pub async fn start(client: &stackable_operator::client::Client) {
    let controller = Controller::new(
        client.get_all_api::<Pod>(),
        watcher::Config::default().labels("enrichment.stackable.tech/enabled=true"),
    );
    let pods = controller.store();
    controller
        .watches(
            client.get_all_api::<Node>(),
            watcher::Config::default(),
            move |node| {
                pods.state()
                    .into_iter()
                    .filter(move |pod| {
                        pod.spec.as_ref().and_then(|s| s.node_name.as_deref())
                            == node.metadata.name.as_deref()
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
        .for_each(|res| async move {
            report_controller_reconciled(client, "pod.enrichment.commons.stackable.tech", &res)
        })
        .await;
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Eq, Ord, strum::EnumString)]
pub enum NodeAddressType {
    ExternalIP,
    InternalIP,
}

async fn reconcile(pod: Arc<Pod>, ctx: Arc<Ctx>) -> Result<controller::Action, Error> {
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

fn error_policy(_obj: Arc<Pod>, _error: &Error, _ctx: Arc<Ctx>) -> controller::Action {
    controller::Action::requeue(Duration::from_secs(5))
}
