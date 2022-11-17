use std::sync::Arc;

use axum::{routing::post, Extension, Json, Router};
use hyper::server::conn::AddrIncoming;
use serde_json::json;
use stackable_operator::{
    k8s_openapi::api::apps::v1::StatefulSet, kube::runtime::controller::Context,
};
use tokio_rustls::rustls;

use crate::utils::{
    tls_cert_manager::{run_cert_manager, ResolvesLatestCert},
    tls_server::TlsAccept,
};

use super::statefulset::{get_updated_restarter_annotations, Ctx};

pub async fn start(ctx: Context<Ctx>) {
    let active_cert = Arc::default();
    let app = Router::new()
        .route("/restarter/webhook", post(webhook))
        .layer(Extension(ctx.clone()));
    let tls_config = Arc::new(
        rustls::server::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(ResolvesLatestCert {
                active_cert: Arc::clone(&active_cert),
            })),
    );
    axum::Server::builder(TlsAccept::new(
        AddrIncoming::bind(&"0.0.0.0:9766".parse().unwrap()).unwrap(),
        tls_config,
    ))
    .serve(app.into_make_service())
    .with_graceful_shutdown(run_cert_manager(&ctx.get_ref().client, active_cert))
    .await
    .unwrap();
}

async fn webhook(
    Json(review): Json<serde_json::Value>,
    Extension(ctx): Extension<Context<Ctx>>,
) -> Json<serde_json::Value> {
    let sts = serde_json::from_value::<StatefulSet>(review["request"]["object"].clone()).unwrap();
    let annotations = get_updated_restarter_annotations(&sts, ctx).unwrap();
    let annotations_path_base = "/spec/template/metadata/annotations";
    let mut annotations_len_so_far = 0;
    let mut current = &review["request"]["object"];
    let patch = annotations_path_base
        .split('/')
        .skip(1)
        .filter_map(|part| {
            annotations_len_so_far += "/".len() + part.len();
            current = &current[part];
            current.is_null().then(|| {
                json!({
                    "op": "add",
                    "path": annotations_path_base[..annotations_len_so_far],
                    "value": {},
                })
            })
        })
        .chain(annotations.into_iter().map(|(k, v)| {
            json!({
                "op": "add",
                "path": format!("{annotations_path_base}/{}", k.replace('/', "~1")),
                "value": v,
            })
        }))
        .collect::<Vec<_>>();
    dbg!(&patch);
    let patch_b64 = base64::encode(serde_json::to_vec(&patch).unwrap());
    Json(json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "response": {
            "uid": review["request"]["uid"],
            "allowed": true,
            "patchType": "JSONPatch",
            "patch": patch_b64,
        },
    }))
}
