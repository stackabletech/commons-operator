//! Webhook server assembly for the commons-operator.
//!
//! Wires together all mutating and conversion webhooks into a single [`WebhookServer`].
//! Each webhook can be independently disabled via CLI flags.

use std::sync::Arc;

use snafu::{ResultExt, Snafu};
use stackable_operator::{
    cli::OperatorEnvironmentOptions,
    kube::Client,
    webhook::{WebhookServer, WebhookServerError, WebhookServerOptions, webhooks::Webhook},
};

use crate::restart_controller::statefulset::Ctx;

mod conversion;
mod restarter_mutate_sts;
mod scaler_admission;

/// Errors from webhook server setup.
#[derive(Debug, Snafu)]
pub enum Error {
    /// The [`WebhookServer`] could not be created (e.g. TLS certificate loading failed).
    #[snafu(display("failed to create webhook server"))]
    CreateWebhookServer { source: WebhookServerError },
}

/// Assemble and return a [`WebhookServer`] with all commons-operator webhooks.
///
/// # Parameters
///
/// - `ctx`: Shared controller context used by the StatefulSet restarter webhook.
/// - `operator_environment`: Namespace and service name for socket address configuration.
/// - `disable_restarter_mutating_webhook`: Skip the StatefulSet restarter webhook.
/// - `disable_scaler_admission_webhook`: Skip the
///   [`StackableScaler`](stackable_operator::crd::scaler::StackableScaler) admission webhook.
/// - `disable_crd_maintenance`: Skip CRD maintenance in the conversion webhook.
/// - `client`: Kubernetes client passed to webhooks needing API access.
pub async fn create_webhook_server(
    ctx: Arc<Ctx>,
    operator_environment: &OperatorEnvironmentOptions,
    disable_restarter_mutating_webhook: bool,
    disable_scaler_admission_webhook: bool,
    disable_crd_maintenance: bool,
    client: Client,
) -> Result<WebhookServer, Error> {
    let mut webhooks: Vec<Box<dyn Webhook>> = vec![];

    if let Some(webhook) = restarter_mutate_sts::create_webhook(
        ctx,
        disable_restarter_mutating_webhook,
        client.clone(),
    ) {
        webhooks.push(webhook);
    }

    if let Some(webhook) =
        scaler_admission::create_webhook(disable_scaler_admission_webhook, client.clone())
    {
        webhooks.push(webhook);
    }

    // TODO (@Techassi): The conversion webhook should also allow to be disabled, rework the
    // granularity of these options.
    webhooks.push(conversion::create_webhook(disable_crd_maintenance, client));

    let webhook_options = WebhookServerOptions {
        socket_addr: WebhookServer::DEFAULT_SOCKET_ADDRESS,
        webhook_namespace: operator_environment.operator_namespace.to_owned(),
        webhook_service_name: operator_environment.operator_service_name.to_owned(),
    };
    WebhookServer::new(webhooks, webhook_options)
        .await
        .context(CreateWebhookServerSnafu)
}
