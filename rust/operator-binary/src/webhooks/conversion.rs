use stackable_operator::{
    crd::{
        authentication::core::{AuthenticationClass, AuthenticationClassVersion},
        openlineage::{OpenLineageConnection, OpenLineageConnectionVersion},
        s3::{S3Bucket, S3BucketVersion, S3Connection, S3ConnectionVersion},
        scaler::{Scaler, ScalerVersion},
    },
    kube::Client,
    webhook::webhooks::{ConversionWebhook, ConversionWebhookOptions, Webhook},
};

use crate::FIELD_MANAGER;

pub fn create_webhook(disable_crd_maintenance: bool, client: Client) -> Box<impl Webhook> {
    let crds_and_handlers = vec![
        (
            AuthenticationClass::merged_crd(AuthenticationClassVersion::V1Alpha1)
                .expect("the AuthenticationClass CRD versions must be mergeable"),
            AuthenticationClass::try_convert as fn(_) -> _,
        ),
        (
            S3Connection::merged_crd(S3ConnectionVersion::V1Alpha1)
                .expect("the S3Connection CRD versions must be mergeable"),
            S3Connection::try_convert as fn(_) -> _,
        ),
        (
            S3Bucket::merged_crd(S3BucketVersion::V1Alpha1)
                .expect("the S3Bucket CRD versions must be mergeable"),
            S3Bucket::try_convert as fn(_) -> _,
        ),
        (
            OpenLineageConnection::merged_crd(OpenLineageConnectionVersion::V1Alpha1)
                .expect("the OpenLineageConnection CRD versions must be mergeable"),
            OpenLineageConnection::try_convert as fn(_) -> _,
        ),
        (
            Scaler::merged_crd(ScalerVersion::V1Alpha1)
                .expect("the Scaler CRD versions must be mergeable"),
            Scaler::try_convert as fn(_) -> _,
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
