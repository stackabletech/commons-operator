use stackable_operator::{
    crd::{
        authentication::core::{AuthenticationClass, AuthenticationClassVersion},
        s3::{S3Bucket, S3BucketVersion, S3Connection, S3ConnectionVersion},
        scaler::{StackableScaler, StackableScalerVersion},
    },
    kube::Client,
    webhook::webhooks::{ConversionWebhook, ConversionWebhookOptions, Webhook},
};

use crate::FIELD_MANAGER;

pub fn create_webhook(disable_crd_maintenance: bool, client: Client) -> Box<impl Webhook> {
    let crds_and_handlers = vec![
        (
            AuthenticationClass::merged_crd(AuthenticationClassVersion::V1Alpha1).unwrap(),
            AuthenticationClass::try_convert as fn(_) -> _,
        ),
        (
            S3Connection::merged_crd(S3ConnectionVersion::V1Alpha1).unwrap(),
            S3Connection::try_convert as fn(_) -> _,
        ),
        (
            S3Bucket::merged_crd(S3BucketVersion::V1Alpha1).unwrap(),
            S3Bucket::try_convert as fn(_) -> _,
        ),
        (
            StackableScaler::merged_crd(StackableScalerVersion::V1Alpha1).unwrap(),
            StackableScaler::try_convert as fn(_) -> _,
        ),
    ];

    let conversion_webhook_options = ConversionWebhookOptions {
        disable_crd_maintenance,
        field_manager: FIELD_MANAGER.to_owned(),
    };

    let (conversion_webhook, _initial_reconcile_rx) =
        ConversionWebhook::new(crds_and_handlers, client, conversion_webhook_options);

    Box::new(conversion_webhook)
}
