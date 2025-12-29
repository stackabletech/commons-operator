use std::{collections::BTreeMap, sync::Arc};

use json_patch::{AddOperation, Patch, PatchOperation, jsonptr::PointerBuf};
use stackable_operator::{
    builder::meta::ObjectMetaBuilder,
    k8s_openapi::{
        api::{
            admissionregistration::v1::{
                MutatingWebhook, MutatingWebhookConfiguration, RuleWithOperations,
                WebhookClientConfig,
            },
            apps::v1::StatefulSet,
        },
        apimachinery::pkg::apis::meta::v1::LabelSelector,
    },
    kube::core::admission::{AdmissionRequest, AdmissionResponse},
    kvp::Label,
};

use crate::{
    OPERATOR_NAME,
    restart_controller::statefulset::{Ctx, get_updated_restarter_annotations},
};

pub fn get_sts_restarter_mutating_webhook_configuration() -> MutatingWebhookConfiguration {
    let webhook_name = "restarter-sts-enricher.stackable.tech";
    let metadata = ObjectMetaBuilder::new()
        .name(webhook_name)
        .with_label(Label::stackable_vendor())
        .with_label(
            Label::managed_by(OPERATOR_NAME, webhook_name).expect("static label is always valid"),
        )
        .build();

    MutatingWebhookConfiguration {
        metadata,
        webhooks: Some(vec![MutatingWebhook {
            name: webhook_name.to_owned(),
            // This is checked by the stackable_webhook code
            admission_review_versions: vec!["v1".to_owned()],
            rules: Some(vec![RuleWithOperations {
                api_groups: Some(vec!["apps".to_owned()]),
                api_versions: Some(vec!["v1".to_owned()]),
                resources: Some(vec!["statefulsets".to_owned()]),
                operations: Some(vec!["CREATE".to_owned()]),
                scope: Some("Namespaced".to_owned()),
            }]),
            // We only need to care about StatefulSets with the `restarter.stackable.tech/enabled``
            // label set to `true`.
            object_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "restarter.stackable.tech/enabled".to_owned(),
                    "true".to_owned(),
                )])),
                match_expressions: None,
            }),
            // Will be set by the stackable_webhook code
            client_config: WebhookClientConfig::default(),
            // Worst case if the annotations are missing they cause a restart of Pod 0, basically
            // the same behavior which we had for years.
            // See https://github.com/stackabletech/commons-operator/issues/111 for details
            failure_policy: Some("Ignore".to_owned()),
            // It could be the case that other mutating webhooks add more ConfigMap/Secret mounts,
            // in which case it would be nice if we detect that.
            reinvocation_policy: Some("IfNeeded".to_owned()),
            // > Webhooks typically operate only on the content of the AdmissionReview sent to them.
            // > Some webhooks, however, make out-of-band changes as part of processing admission requests.
            //
            // We read in the state of the world using the ConfigMap and Secret store.
            // So, technically our outcome depends on external factors, *but* this webhook is not
            // creating any external objects, so from our understanding it's side-effect free.
            side_effects: "None".to_owned(),
            ..Default::default()
        }]),
    }
}

pub async fn add_sts_restarter_annotations_handler(
    ctx: Arc<Ctx>,
    request: AdmissionRequest<StatefulSet>,
) -> AdmissionResponse {
    let Some(sts) = &request.object else {
        return AdmissionResponse::invalid(
            "object (of type StatefulSet) missing - for operation CREATE it must be always present",
        );
    };

    let mut paths_to_be_created = vec![];
    let spec = sts.spec.as_ref();
    if spec.is_none() {
        paths_to_be_created.push("/spec");
    }
    let metadata = spec.and_then(|spec| spec.template.metadata.as_ref());
    if metadata.is_none() {
        paths_to_be_created.push("/spec/template/metadata");
    }
    let annotations = metadata.and_then(|metadata| metadata.annotations.as_ref());
    if annotations.is_none() {
        paths_to_be_created.push("/spec/template/metadata/annotations");
    }
    let create_paths = paths_to_be_created.into_iter().map(|path| {
        PatchOperation::Add(AddOperation {
            path: PointerBuf::parse(path).expect("hard-coded annotation paths are valid"),
            value: serde_json::Value::Object(serde_json::Map::new()),
        })
    });

    let annotations = match get_updated_restarter_annotations(sts, ctx).await {
        Ok(annotations) => annotations,
        Err(err) => {
            return AdmissionResponse::invalid(format!(
                "failed to get updated restarted annotations: {err:#}"
            ));
        }
    };

    let add_annotations = annotations.iter().map(|(k, v)| {
        PatchOperation::Add(AddOperation {
            path: PointerBuf::from_tokens([
                "spec",
                "template",
                "metadata",
                "annotations",
                // It's totally fine (and even expected) that the annotations contains slashes ("/"),
                // as `PointerBuf::from_tokens` escapes them
                k,
            ]),
            value: serde_json::Value::String(v.to_owned()),
        })
    });

    match AdmissionResponse::from(&request)
        .with_patch(Patch(create_paths.chain(add_annotations).collect()))
    {
        Ok(response) => response,
        Err(err) => {
            AdmissionResponse::invalid(format!("failed to add patch to AdmissionResponse: {err:#}"))
        }
    }
}
