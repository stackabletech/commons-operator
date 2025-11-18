use std::sync::Arc;

use restarter_mutate_sts::{add_sts_restarter_annotation, get_mutating_webhook_configuration};
use snafu::{ResultExt, Snafu};
use stackable_operator::{
    cli::OperatorEnvironmentOptions,
    kube::Client,
    webhook::{
        WebhookError, WebhookOptions, WebhookServer,
        servers::{MutatingWebhookServer, WebhookServerImplementation},
    },
};

use crate::{FIELD_MANAGER, restart_controller::statefulset::Ctx};

mod restarter_mutate_sts;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to create webhook server"))]
    CreateWebhookServer { source: WebhookError },
}

pub async fn create_webhook<'a>(
    ctx: Arc<Ctx>,
    operator_environment: &'a OperatorEnvironmentOptions,
    disable_restarter_mutating_webhook: bool,
    client: Client,
) -> Result<WebhookServer, Error> {
    let mut webhooks: Vec<Box<dyn WebhookServerImplementation>> = vec![];
    if !disable_restarter_mutating_webhook {
        webhooks.push(Box::new(MutatingWebhookServer::new(
            get_mutating_webhook_configuration(),
            add_sts_restarter_annotation,
            ctx,
            disable_restarter_mutating_webhook,
            client,
            FIELD_MANAGER.to_owned(),
        )));
    }

    let webhook_options = WebhookOptions {
        socket_addr: WebhookServer::DEFAULT_SOCKET_ADDRESS,
        operator_namespace: operator_environment.operator_namespace.to_owned(),
        operator_service_name: operator_environment.operator_service_name.to_owned(),
    };
    WebhookServer::new(webhook_options, webhooks)
        .await
        .context(CreateWebhookServerSnafu)
}
