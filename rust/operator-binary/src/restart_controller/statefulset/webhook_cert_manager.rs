use std::{
    io::Cursor,
    sync::{Arc, RwLock},
};

use futures::{future, pin_mut, StreamExt};
use openssl::{
    asn1::{Asn1Integer, Asn1Time},
    bn::{BigNum, MsbOption},
    conf::{Conf, ConfMethod},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    x509::{
        extension::{
            AuthorityKeyIdentifier, BasicConstraints, KeyUsage, SubjectAlternativeName,
            SubjectKeyIdentifier,
        },
        X509Builder, X509NameBuilder, X509,
    },
};
use rustls_pemfile::{certs, pkcs8_private_keys};
use serde_json::json;
use stackable_operator::{
    builder::ObjectMetaBuilder,
    k8s_openapi::{
        api::{admissionregistration::v1::MutatingWebhookConfiguration, core::v1::Secret},
        chrono::{DateTime, Utc},
        ByteString,
    },
    kube::runtime::{reflector::ObjectRef, watcher::watch_object},
};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use tokio::{sync::watch, time::Instant};
use tokio_rustls::rustls::{
    self, server::ResolvesServerCert, sign::CertifiedKey, Certificate, PrivateKey,
};
use tokio_stream::wrappers::WatchStream;
use tracing::{info, warn};

use crate::utils::single_object_controller::{single_object_applier, LastObjectEmitter};

pub async fn start(
    client: &stackable_operator::client::Client,
    active_cert_keypair: Arc<RwLock<Option<Arc<CertifiedKey>>>>,
) {
    let (active_cert_pem_tx, active_cert_pm_rx) = watch::channel(None);
    let cert_manager = cert_manager(client, active_cert_keypair, active_cert_pem_tx);
    let webhook_cert_publisher = webhook_cert_publisher(
        client,
        ObjectRef::<MutatingWebhookConfiguration>::new("restarter.stackable.tech"),
        active_cert_pm_rx,
    );
    pin_mut!(cert_manager, webhook_cert_publisher);
    future::select(cert_manager, webhook_cert_publisher).await;
}

async fn cert_manager(
    client: &stackable_operator::client::Client,
    active_cert_keypair: Arc<RwLock<Option<Arc<CertifiedKey>>>>,
    active_cert_pem: watch::Sender<Option<Vec<u8>>>,
) {
    let secret_name = &"asdfasdf-webhook-cert";
    single_object_applier(
        watch_object(client.get_namespaced_api::<Secret>("default"), secret_name)
            .map(Result::unwrap),
        |old_secret| async {
            let old_secret = old_secret;

            let (old_cert_keypair, old_cert_pem) = old_secret
                .as_deref()
                .map(decode_keypair_from_secret)
                .map_or((None, None), |(kp, pem)| (Some(kp), Some(pem)));
            *active_cert_keypair.write().unwrap() = old_cert_keypair.map(Arc::new);
            let _ = active_cert_pem.send(old_cert_pem.map(|s| s.to_vec()));
            if old_secret.is_some() {
                info!("tls cert loaded");
            } else {
                info!("no tls cert found");
            }

            let renew_after = old_secret
                .as_ref()
                .and_then(|sec| {
                    sec.metadata
                        .annotations
                        .as_ref()?
                        .get("internal.restarter.stackable.tech/renew-after")
                })
                .map(|renew_after| DateTime::parse_from_rfc3339(renew_after).unwrap());
            let should_renew = renew_after.map_or(true, |r| r < Utc::now());

            if should_renew {
                let now = OffsetDateTime::now_utc();
                let lifetime = Duration::hours(1);
                let expires_at = now + lifetime;
                let renew_after = now + lifetime / 2;
                let (key, cert) = generate_cert(expires_at);
                info!("renewing webhook cert");
                let new_secret = Secret {
                    metadata: ObjectMetaBuilder::new()
                        .name(*secret_name)
                        .namespace("default")
                        .with_annotation(
                            "internal.restarter.stackable.tech/renew-after",
                            renew_after.format(&Rfc3339).unwrap(),
                        )
                        .build(),
                    data: Some(
                        [
                            (
                                "tls.key".to_string(),
                                ByteString(key.private_key_to_pem_pkcs8().unwrap()),
                            ),
                            ("tls.crt".to_string(), ByteString(cert.to_pem().unwrap())),
                        ]
                        .into(),
                    ),
                    ..Default::default()
                };
                client
                    .apply_patch("asdfasdf", &new_secret, &new_secret)
                    .await
                    .unwrap();
            }

            renew_after.and_then(|dt| {
                Some(Instant::now() + (dt.with_timezone(&Utc) - Utc::now()).to_std().ok()?)
            })
        },
    )
    .collect::<()>()
    .await;
}

async fn webhook_cert_publisher(
    client: &stackable_operator::client::Client,
    webhook: ObjectRef<MutatingWebhookConfiguration>,
    cert_pem: watch::Receiver<Option<Vec<u8>>>,
) {
    single_object_applier(
        LastObjectEmitter::new(
            watch_object(
                client.get_all_api::<MutatingWebhookConfiguration>(),
                &webhook.name,
            )
            .map(Result::unwrap),
            WatchStream::new(cert_pem.clone()).map(|_| ()),
        ),
        |mwc| async {
            if let Some((mwc, cert)) = mwc.zip(cert_pem.borrow().as_deref()) {
                let mut hooks = vec![];
                for hook in mwc.webhooks.iter().flatten() {
                    let cert_b64 = base64::encode(cert);
                    hooks.push(json!({
                        "name": &hook.name,
                        "clientConfig": {
                            "caBundle": cert_b64,
                        }
                    }))
                }
                client
                    .apply_patch(
                        "asdfasdf",
                        &*mwc,
                        // k8s-openapi's type defitions aren't properly Optional for values that we
                        // expect to have been written at deploy-time.
                        json!({
                            "apiVersion": "admissionregistration.k8s.io/v1",
                            "kind": "MutatingWebhookConfiguration",
                            "webhooks": hooks,
                        }),
                    )
                    .await
                    .unwrap();
            }
            None
        },
    )
    .collect::<()>()
    .await;
}

fn decode_keypair_from_secret(secret: &Secret) -> (CertifiedKey, &[u8]) {
    let data = secret.data.as_ref().unwrap();
    let cert = &data["tls.crt"].0;
    (
        CertifiedKey::new(
            certs(&mut Cursor::new(cert))
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
        ),
        cert,
    )
}

fn generate_cert(not_after: OffsetDateTime) -> (PKey<Private>, X509) {
    // TODO: based on secret-operator's cert generation code, clean up?
    let subject_name = X509NameBuilder::new()
        .and_then(|mut name| {
            name.append_entry_by_nid(Nid::COMMONNAME, "commons-operator webhook certificate")?;
            Ok(name)
        })
        .unwrap()
        .build();
    let now = OffsetDateTime::now_utc();
    let not_before = now - Duration::minutes(5);
    let conf = Conf::new(ConfMethod::default()).unwrap();
    let key = Rsa::generate(2048).and_then(PKey::try_from).unwrap();
    let cert = X509Builder::new()
        .and_then(|mut x509| {
            x509.set_subject_name(&subject_name)?;
            x509.set_issuer_name(&subject_name)?;
            x509.set_not_before(Asn1Time::from_unix(not_before.unix_timestamp())?.as_ref())?;
            x509.set_not_after(Asn1Time::from_unix(not_after.unix_timestamp())?.as_ref())?;
            x509.set_pubkey(&key)?;
            let mut serial = BigNum::new()?;
            serial.rand(64, MsbOption::MAYBE_ZERO, false)?;
            x509.set_serial_number(Asn1Integer::from_bn(&serial)?.as_ref())?;
            x509.set_version(
                3 - 1, // zero-indexed
            )?;
            let ctx = x509.x509v3_context(None, Some(&conf));
            let exts = [
                BasicConstraints::new().critical().build()?,
                SubjectKeyIdentifier::new().build(&ctx)?,
                AuthorityKeyIdentifier::new()
                    .issuer(false)
                    .keyid(false)
                    .build(&ctx)?,
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .key_cert_sign()
                    .crl_sign()
                    .build()?,
                SubjectAlternativeName::new()
                    .critical()
                    .ip("192.168.1.147")
                    .build(&ctx)
                    .unwrap(),
            ];
            for ext in exts {
                x509.append_extension(ext)?;
            }
            x509.sign(&key, MessageDigest::sha256())?;
            Ok(x509)
        })
        .unwrap()
        .build();
    (key, cert)
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
