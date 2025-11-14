use std::collections::BTreeMap;

use snafu::{ResultExt, Snafu};
use stackable_operator::{
    builder::meta::ObjectMetaBuilder,
    cli::OperatorEnvironmentOptions,
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
    kube::{
        Client,
        core::admission::{AdmissionRequest, AdmissionResponse},
    },
    kvp::Label,
    webhook::{WebhookError, WebhookOptions, WebhookServer, servers::MutatingWebhookServer},
};

use crate::{FIELD_MANAGER, OPERATOR_NAME};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to create webhook server"))]
    CreateWebhookServer { source: WebhookError },
}

pub async fn create_webhook<'a>(
    operator_environment: &'a OperatorEnvironmentOptions,
    disable_mutating_webhook_configuration_maintenance: bool,
    client: Client,
) -> Result<WebhookServer, Error> {
    let mutating_webhook_server = MutatingWebhookServer::new(
        get_mutating_webhook_configuration(),
        foo,
        disable_mutating_webhook_configuration_maintenance,
        client,
        FIELD_MANAGER.to_owned(),
    );

    let webhook_options = WebhookOptions {
        socket_addr: WebhookServer::DEFAULT_SOCKET_ADDRESS,
        operator_namespace: operator_environment.operator_namespace.to_owned(),
        operator_service_name: operator_environment.operator_service_name.to_owned(),
    };
    WebhookServer::new(webhook_options, vec![Box::new(mutating_webhook_server)])
        .await
        .context(CreateWebhookServerSnafu)
}

fn get_mutating_webhook_configuration() -> MutatingWebhookConfiguration {
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
            // Worst case the annotations are missing cause a restart of Pod 0, basically the same
            // behavior which we had for years.
            // See https://github.com/stackabletech/commons-operator/issues/111 for details
            // failure_policy: Some("Ignore".to_owned()),
            // TEMP for testing
            failure_policy: Some("Fail".to_owned()),
            // It could be the case that other mutating webhooks add more ConfigMpa/Secret mounts,
            // in which case it would be nice if we detect that.
            reinvocation_policy: Some("IfNeeded".to_owned()),
            // We don't have side effects
            side_effects: "None".to_owned(),
            ..Default::default()
        }]),
    }
}

fn foo(request: AdmissionRequest<StatefulSet>) -> AdmissionResponse {
    let Some(sts) = &request.object else {
        return AdmissionResponse::invalid(
            "object (of type StatefulSet) missing - for operation CREATE it must be always present",
        );
    };

    dbg!(&request);

    AdmissionResponse::from(&request)
}
