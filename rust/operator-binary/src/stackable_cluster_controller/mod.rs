pub mod crd;
pub mod secret_operator;

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    builder::OwnerReferenceBuilder,
    client::{Client, GetApi},
    error::OperatorResult,
    k8s_openapi::{
        api::{
            apps::v1::DaemonSet,
            storage::v1::{CSIDriver, StorageClass},
        },
        apimachinery::pkg::apis::meta::v1::OwnerReference,
    },
    kube::{
        api::ListParams,
        runtime::{controller, reflector::ObjectRef, Controller},
        Resource, ResourceExt,
    },
    logging::controller::{report_controller_reconciled, ReconcilerError},
    namespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

use self::crd::StackableCluster;

static STACKABLE_CLUSTER_CR_NAME: &str = "stackable-cluster";
const FIELD_MANAGER_SCOPE: &str = "commons.stackable.tech/stackablecluster";

struct Ctx {
    client: stackable_operator::client::Client,
    namespace: String,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
pub enum Error {
    #[snafu(display(
        "there can be only a single StackableCluster called {STACKABLE_CLUSTER_CR_NAME}"
    ))]
    OnlySingleStackableCluster {},
    #[snafu(display("failed to parse yaml manifest"))]
    ParseManifest { source: serde_yaml::Error },
    #[snafu(display("failed to update Daemonset"))]
    UpdateDaemonset {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to update CSIDriver"))]
    UpdateCSIDriver {
        source: stackable_operator::error::Error,
    },
    #[snafu(display("failed to update StorageClass"))]
    UpdateStorageClass {
        source: stackable_operator::error::Error,
    },
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<stackable_operator::kube::core::DynamicObject>> {
        match self {
            Error::OnlySingleStackableCluster {} => None,
            Error::ParseManifest { .. } => None,
            Error::UpdateDaemonset { .. } => None,
            Error::UpdateCSIDriver { .. } => None,
            Error::UpdateStorageClass { .. } => None,
        }
    }
}

pub async fn start(client: &stackable_operator::client::Client, namespace: String) {
    let controller = Controller::new(
        client.get_all_api::<StackableCluster>(),
        ListParams::default(),
    );
    controller
        .run(
            reconcile,
            error_policy,
            Arc::new(Ctx {
                client: client.clone(),
                namespace,
            }),
        )
        .for_each(|res| async move {
            report_controller_reconciled(client, "pod.enrichment.commons.stackable.tech", &res)
        })
        .await;
}

async fn reconcile(
    stackable_cluster: Arc<StackableCluster>,
    ctx: Arc<Ctx>,
) -> Result<controller::Action, Error> {
    if stackable_cluster.name_any() != STACKABLE_CLUSTER_CR_NAME {
        return OnlySingleStackableClusterSnafu.fail();
    }

    let csi_driver: CSIDriver =
        serde_yaml::from_str(include_str!("secret_operator/manifests/csidriver.yaml"))
            .context(ParseManifestSnafu)?;
    add_owner_reference_and_patch(&ctx.client, csi_driver, &stackable_cluster)
        .await
        .context(UpdateCSIDriverSnafu)?;

    let storage_class: StorageClass =
        serde_yaml::from_str(include_str!("secret_operator/manifests/storageclass.yaml"))
            .context(ParseManifestSnafu)?;
    add_owner_reference_and_patch(&ctx.client, storage_class, &stackable_cluster)
        .await
        .context(UpdateStorageClassSnafu)?;

    let mut daemon_set: DaemonSet =
        serde_yaml::from_str(include_str!("secret_operator/manifests/daemonset.yaml"))
            .context(ParseManifestSnafu)?;
    daemon_set.metadata.namespace = Some(ctx.namespace.clone());
    add_owner_reference_and_patch(&ctx.client, daemon_set, &stackable_cluster)
        .await
        .context(UpdateDaemonsetSnafu)?;

    Ok(controller::Action::await_change())
}

async fn add_owner_reference_and_patch<T>(
    client: &Client,
    mut resource: T,
    stackable_cluster: &StackableCluster,
) -> OperatorResult<T>
where
    T: Clone + std::fmt::Debug + Serialize + DeserializeOwned + Resource + GetApi,
    <T as Resource>::DynamicType: Default,
{
    resource.owner_references_mut().push(
        OwnerReferenceBuilder::new()
            .initialize_from_resource(stackable_cluster)
            .build()
            .unwrap(),
    );

    client
        .apply_patch(FIELD_MANAGER_SCOPE, &resource, &resource)
        .await
}

fn error_policy(_obj: Arc<StackableCluster>, _error: &Error, _ctx: Arc<Ctx>) -> controller::Action {
    controller::Action::requeue(Duration::from_secs(5))
}
