//! Admission webhook for [`StackableScaler`] resources.
//!
//! This webhook serves two purposes:
//!
//! ## Validation
//! On `UPDATE` operations, rejects changes to `spec.replicas` while a scaling operation
//! is in progress (stage is not `Idle` or `Failed`). Because Kubernetes strips `.status`
//! from `oldObject` for CRDs with a status subresource, the webhook fetches the live
//! object to inspect the current stage.
//!
//! ## Mutation
//! Injects the `stackable.tech/cluster-kind` label from `spec.clusterRef.kind` so that
//! cluster-scoped label selectors work without requiring clients to set the label manually.

use std::sync::Arc;

use json_patch::{AddOperation, Patch, PatchOperation, jsonptr::PointerBuf};
use stackable_operator::{
    builder::meta::ObjectMetaBuilder,
    crd::scaler::v1alpha1::StackableScaler,
    k8s_openapi::api::admissionregistration::v1::{
        MutatingWebhook, MutatingWebhookConfiguration, RuleWithOperations, WebhookClientConfig,
    },
    kube::{
        Api, Client,
        core::admission::{AdmissionRequest, AdmissionResponse, Operation},
    },
    kvp::Label,
    webhook::webhooks::{MutatingWebhookOptions, Webhook},
};
use tracing::{debug, info, warn};

use crate::{FIELD_MANAGER, OPERATOR_NAME};

/// Create the [`StackableScaler`] admission webhook, or `None` if disabled.
///
/// # Parameters
///
/// - `disable`: When `true`, the webhook is not started and `None` is returned.
///   Corresponds to the `--disable-scaler-admission-webhook` CLI flag.
/// - `client`: Kubernetes client used by the handler to fetch live [`StackableScaler`]
///   objects during admission review.
pub fn create_webhook(disable: bool, client: Client) -> Option<Box<impl Webhook>> {
    (!disable).then(|| {
        let options = MutatingWebhookOptions {
            // When `disable` is true the outer `(!disable).then(...)` returns None,
            // so inside this closure `disable` is always false.
            disable_mwc_maintenance: false,
            field_manager: FIELD_MANAGER.to_owned(),
        };

        Box::new(stackable_operator::webhook::webhooks::MutatingWebhook::new(
            get_scaler_admission_webhook_configuration(),
            scaler_admission_handler,
            Arc::new(client.clone()),
            client,
            options,
        ))
    })
}

/// Build the [`MutatingWebhookConfiguration`] for the scaler admission webhook.
///
/// Covers `CREATE` and `UPDATE` operations on `stackablescalers.autoscaling.stackable.tech`.
/// `failure_policy` is `Fail` because an unenforced replicas change during active scaling
/// would corrupt the scaler state machine.
fn get_scaler_admission_webhook_configuration() -> MutatingWebhookConfiguration {
    let webhook_name = "scaler-admission.stackable.tech";
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
            admission_review_versions: vec!["v1".to_owned()],
            rules: Some(vec![RuleWithOperations {
                api_groups: Some(vec!["autoscaling.stackable.tech".to_owned()]),
                api_versions: Some(vec!["v1alpha1".to_owned()]),
                resources: Some(vec!["stackablescalers".to_owned()]),
                operations: Some(vec!["CREATE".to_owned(), "UPDATE".to_owned()]),
                scope: Some("Namespaced".to_owned()),
            }]),
            client_config: WebhookClientConfig::default(),
            // TODO(#3): `Fail` blocks all HPA `/scale` writes when the commons-operator is
            // unavailable (rolling update, crash). The restarter webhook uses `Ignore` for
            // this reason. Consider switching to `Ignore` or narrowing scope with an
            // `object_selector` to limit blast radius.
            failure_policy: Some("Fail".to_owned()),
            // TODO(#4): The restarter webhook uses `IfNeeded` with a documented rationale.
            // Explain why this webhook uses `Never`, or switch to `IfNeeded` for consistency.
            reinvocation_policy: Some("Never".to_owned()),
            side_effects: "None".to_owned(),
            ..Default::default()
        }]),
    }
}

/// Handle an admission request for a [`StackableScaler`] resource.
///
/// On `UPDATE`: if `spec.replicas` changed, fetches the live object (5s timeout) and
/// denies the change unless the stage is `Idle`, `Failed`, or absent.
///
/// On all operations: injects `stackable.tech/cluster-kind` label after validating
/// the value is a legal Kubernetes label.
///
/// # Parameters
///
/// - `client`: Used to fetch the live object when validating an `UPDATE`.
/// - `request`: The incoming admission review request.
async fn scaler_admission_handler(
    client: Arc<Client>,
    request: AdmissionRequest<StackableScaler>,
) -> AdmissionResponse {
    let Some(scaler) = &request.object else {
        warn!(
            operation = ?request.operation,
            "Denying admission: StackableScaler object missing from request"
        );
        return AdmissionResponse::from(&request).deny("object (of type StackableScaler) missing");
    };

    let Some(scaler_name) = scaler.metadata.name.as_deref() else {
        warn!(
            operation = ?request.operation,
            "Denying admission: StackableScaler is missing metadata.name"
        );
        return AdmissionResponse::from(&request).deny("StackableScaler is missing metadata.name");
    };
    let Some(scaler_namespace) = scaler.metadata.namespace.as_deref() else {
        warn!(
            scaler = scaler_name,
            operation = ?request.operation,
            "Denying admission: StackableScaler is missing metadata.namespace"
        );
        return AdmissionResponse::from(&request)
            .deny("StackableScaler is missing metadata.namespace");
    };

    debug!(
        scaler = scaler_name,
        namespace = scaler_namespace,
        operation = ?request.operation,
        "Processing scaler admission request"
    );

    // --- Validation ---
    // On UPDATE: reject spec.replicas changes during active scaling.
    // Kubernetes strips .status from oldObject in admission requests for CRDs
    // with a status subresource, so we fetch the live object to check the stage.
    if request.operation == Operation::Update {
        if let Some(old) = &request.old_object {
            if scaler.spec.replicas != old.spec.replicas {
                let api: Api<StackableScaler> =
                    Api::namespaced((*client).clone(), scaler_namespace);

                match tokio::time::timeout(std::time::Duration::from_secs(5), api.get(scaler_name))
                    .await
                {
                    Ok(Ok(live)) => {
                        let stage = live
                            .status
                            .as_ref()
                            .and_then(|s| s.current_state.as_ref())
                            .map(|state| &state.stage);

                        let is_safe =
                            !stage.is_some_and(|s| s.is_scaling_in_progress());

                        if !is_safe {
                            let stage_str = stage
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "unknown".to_string());
                            info!(
                                scaler = scaler_name,
                                namespace = scaler_namespace,
                                stage = %stage_str,
                                old_replicas = old.spec.replicas,
                                new_replicas = scaler.spec.replicas,
                                "Denying spec.replicas change while scaling is in progress"
                            );
                            return AdmissionResponse::from(&request).deny(format!(
                                "Cannot update spec.replicas while scaling is in progress (stage: {stage_str})"
                            ));
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(
                            scaler = scaler_name,
                            namespace = scaler_namespace,
                            error = %e,
                            "Denying admission: failed to fetch live StackableScaler to verify scaling state"
                        );
                        return AdmissionResponse::from(&request)
                            .deny(format!("Cannot verify scaling state: {e}"));
                    }
                    Err(_) => {
                        warn!(
                            scaler = scaler_name,
                            namespace = scaler_namespace,
                            "Denying admission: timed out fetching live StackableScaler to verify scaling state"
                        );
                        return AdmissionResponse::from(&request).deny(
                            "Cannot verify scaling state: timed out fetching live StackableScaler",
                        );
                    }
                }
            }
        }
    }

    // --- Mutation ---
    // Inject cluster-kind label from spec.clusterRef.kind
    let cluster_kind = &scaler.spec.cluster_ref.kind;

    // Validate the label value before injecting
    if let Err(e) = Label::try_from(("stackable.tech/cluster-kind", cluster_kind.as_str())) {
        warn!(
            scaler = scaler_name,
            namespace = scaler_namespace,
            cluster_kind = %cluster_kind,
            error = %e,
            "Denying admission: clusterRef.kind is not a valid Kubernetes label value"
        );
        return AdmissionResponse::from(&request).deny(format!(
            "clusterRef.kind '{}' is not a valid Kubernetes label value: {e}",
            cluster_kind
        ));
    }

    let mut patches = Vec::new();

    if scaler.metadata.labels.is_none() {
        patches.push(PatchOperation::Add(AddOperation {
            path: PointerBuf::parse("/metadata/labels").expect("valid path"),
            value: serde_json::Value::Object(serde_json::Map::new()),
        }));
    }

    patches.push(PatchOperation::Add(AddOperation {
        path: PointerBuf::from_tokens(["metadata", "labels", "stackable.tech/cluster-kind"]),
        value: serde_json::Value::String(cluster_kind.clone()),
    }));

    match AdmissionResponse::from(&request).with_patch(Patch(patches)) {
        Ok(response) => {
            debug!(
                scaler = scaler_name,
                namespace = scaler_namespace,
                cluster_kind = %cluster_kind,
                "Admitted StackableScaler with cluster-kind label mutation"
            );
            response
        }
        Err(err) => {
            warn!(
                scaler = scaler_name,
                namespace = scaler_namespace,
                error = %err,
                "Denying admission: failed to construct JSON patch"
            );
            AdmissionResponse::from(&request)
                .deny(format!("failed to add patch to AdmissionResponse: {err:#}"))
        }
    }
}
