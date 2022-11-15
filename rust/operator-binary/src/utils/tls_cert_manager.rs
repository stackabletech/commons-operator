use std::{
    io::Cursor,
    sync::{Arc, RwLock},
};

use futures::TryStreamExt;
use rustls_pemfile::{certs, pkcs8_private_keys};
use stackable_operator::{
    k8s_openapi::{
        api::core::v1::Secret,
        chrono::{DateTime, Utc},
    },
    kube::runtime::watcher::watch_object,
};
use tokio_rustls::rustls::{
    self, server::ResolvesServerCert, sign::CertifiedKey, Certificate, PrivateKey,
};
use tracing::{error, info, warn};

pub async fn run_cert_manager(
    client: &stackable_operator::client::Client,
    active_cert: Arc<RwLock<Option<Arc<CertifiedKey>>>>,
) {
    watch_object(
        client.get_namespaced_api::<Secret>("default"),
        "asdfasdf-webhook-cert",
    )
    .and_then(|old_secret| async {
        let old_secret = old_secret;

        *active_cert.write().unwrap() = old_secret
            .as_ref()
            .map(decode_keypair_from_secret)
            .map(Arc::new);
        if old_secret.is_some() {
            info!("tls cert loaded");
        } else {
            info!("no tls cert found");
        }

        let should_renew = old_secret
            .as_ref()
            .and_then(|sec| sec.data.as_ref()?.get("renew-after"))
            .map_or(true, |renew_before| {
                DateTime::parse_from_rfc3339(std::str::from_utf8(&renew_before.0).unwrap()).unwrap()
                    < Utc::now()
            });

        if should_renew {
            error!("TODO: renew")
        }

        Ok(())
    })
    .try_collect::<()>()
    .await
    .unwrap();
}

fn decode_keypair_from_secret(secret: &Secret) -> CertifiedKey {
    let data = secret.data.as_ref().unwrap();
    CertifiedKey::new(
        certs(&mut Cursor::new(&data["tls.crt"].0))
            .unwrap()
            .into_iter()
            .map(Certificate)
            .collect(),
        rustls::sign::any_supported_type(&PrivateKey(
            pkcs8_private_keys(&mut Cursor::new(&data["tls.key"].0))
                .unwrap()
                .remove(0),
        ))
        .unwrap(),
    )
}

pub struct ResolvesLatestCert {
    pub active_cert: Arc<RwLock<Option<Arc<CertifiedKey>>>>,
}
impl ResolvesServerCert for ResolvesLatestCert {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let cert = self.active_cert.read().unwrap().clone();
        if cert.is_none() {
            warn!("tls handshake dropped because no cert is configured yet")
        }
        cert
    }
}
