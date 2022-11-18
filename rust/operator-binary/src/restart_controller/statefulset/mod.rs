use futures::pin_mut;

mod controller;
mod webhook;
mod webhook_cert_manager;

pub async fn start(client: &stackable_operator::client::Client) {
    let (controller, ctx) = controller::start(client);
    let webhook = webhook::start(ctx);
    pin_mut!(controller, webhook);
    futures::future::select(controller, webhook).await;
}
