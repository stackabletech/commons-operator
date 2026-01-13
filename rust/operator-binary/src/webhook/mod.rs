use std::sync::Arc;

use restarter_mutate_sts::{
    add_sts_restarter_annotations_handler, get_sts_restarter_mutating_webhook_configuration,
};
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    cli::OperatorEnvironmentOptions,
    kube::Client,
    webhook::{
        WebhookServer, WebhookServerError, WebhookServerOptions,
        webhooks::{MutatingWebhook, MutatingWebhookOptions, Webhook},
    },
};

use crate::{FIELD_MANAGER, restart_controller::statefulset::Ctx};

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
    client: Client,
) -> Result<WebhookServer, Error> {
    let mut webhooks: Vec<Box<dyn Webhook>> = vec![];
    if !disable_restarter_mutating_webhook {
        let mutating_webhook_options = MutatingWebhookOptions {
            disable_mwc_maintenance: disable_restarter_mutating_webhook,
            field_manager: FIELD_MANAGER.to_owned(),
        };

        webhooks.push(Box::new(MutatingWebhook::new(
            get_sts_restarter_mutating_webhook_configuration(),
            add_sts_restarter_annotations_handler,
            ctx,
            client,
            mutating_webhook_options,
        )));
    }

    let webhook_options = WebhookServerOptions {
        socket_addr: WebhookServer::DEFAULT_SOCKET_ADDRESS,
        webhook_namespace: operator_environment.operator_namespace.to_owned(),
        webhook_service_name: operator_environment.operator_service_name.to_owned(),
    };
    WebhookServer::new(webhooks, webhook_options)
        .await
        .context(CreateWebhookServerSnafu)
}
