use std::sync::Arc;

use snafu::{ResultExt, Snafu};
use stackable_operator::{
    cli::OperatorEnvironmentOptions,
    kube::Client,
    webhook::{WebhookServer, WebhookServerError, WebhookServerOptions, webhooks::Webhook},
};
use tokio::sync::oneshot;

use crate::restart_controller::statefulset::Ctx;

mod conversion;
mod restarter_mutate_sts;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to create webhook server"))]
    CreateWebhookServer { source: WebhookServerError },
}

pub async fn create_webhook_server(
    ctx: Arc<Ctx>,
    operator_environment: &OperatorEnvironmentOptions,
    disable_restarter_mutating_webhook: bool,
    disable_crd_maintenance: bool,
    client: Client,
) -> Result<(WebhookServer, oneshot::Receiver<()>), Error> {
    let mut webhooks: Vec<Box<dyn Webhook>> = vec![];

    if let Some(webhook) = restarter_mutate_sts::create_webhook(
        ctx,
        disable_restarter_mutating_webhook,
        client.clone(),
    ) {
        webhooks.push(webhook);
    }

    // TODO (@Techassi): The conversion webhook should also allow to be disabled, rework the
    // granularity of these options.
    let (webhook, initial_reconcile_rx) =
        conversion::create_webhook(disable_crd_maintenance, client);
    webhooks.push(webhook);

    let webhook_options = WebhookServerOptions {
        socket_addr: WebhookServer::DEFAULT_SOCKET_ADDRESS,
        webhook_namespace: operator_environment.operator_namespace.to_owned(),
        webhook_service_name: operator_environment.operator_service_name.to_owned(),
    };

    let webhook_server = WebhookServer::new(webhooks, webhook_options)
        .await
        .context(CreateWebhookServerSnafu)?;

    Ok((webhook_server, initial_reconcile_rx))
}
