use serde::{Deserialize, Serialize};
use stackable_operator::{
    kube::CustomResource,
    schemars::{self, JsonSchema},
};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "stackableclusters.stackable.tech",
    version = "v1alpha1",
    kind = "StackableCluster",
    crates(
        kube_core = "stackable_operator::kube::core",
        k8s_openapi = "stackable_operator::k8s_openapi",
        schemars = "stackable_operator::schemars"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct StackableClusterSpec {}
