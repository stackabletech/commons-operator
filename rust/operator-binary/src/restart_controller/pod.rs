use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    client::Client,
    k8s_openapi::{
        api::core::v1::Pod,
        chrono::{self, DateTime, FixedOffset, Utc},
    },
    kube::{
        self,
        api::EvictParams,
        core::DynamicObject,
        runtime::{controller::Action, reflector::ObjectRef, watcher, Controller},
    },
    logging::controller::{report_controller_reconciled, ReconcilerError},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

struct Ctx {
    client: Client,
}

#[derive(Snafu, Debug, EnumDiscriminants)]
#[strum_discriminants(derive(IntoStaticStr))]
enum Error {
    #[snafu(display("Pod has no name"))]
    PodHasNoName,
    #[snafu(display("Pod has no namespace"))]
    PodHasNoNamespace,
    #[snafu(display(
        "failed to parse expiry timestamp annotation ({annotation:?}: {value:?}) as RFC 3999"
    ))]
    UnparseableExpiryTimestamp {
        source: chrono::ParseError,
        annotation: String,
        value: String,
    },
    #[snafu(display("failed to evict Pod"))]
    EvictPod { source: kube::Error },
}

impl ReconcilerError for Error {
    fn category(&self) -> &'static str {
        ErrorDiscriminants::from(self).into()
    }

    fn secondary_object(&self) -> Option<ObjectRef<DynamicObject>> {
        match self {
            Error::PodHasNoName => None,
            Error::PodHasNoNamespace => None,
            Error::UnparseableExpiryTimestamp {
                source: _,
                annotation: _,
                value: _,
            } => None,
            Error::EvictPod { source: _ } => None,
        }
    }
}

pub async fn start(client: &Client, watch_namespace: &WatchNamespace) {
    let controller = Controller::new(
        watch_namespace.get_api::<Pod>(client),
        watcher::Config::default(),
    );
    controller
        .run(
            reconcile,
            error_policy,
            Arc::new(Ctx {
                client: client.clone(),
            }),
        )
        .for_each(|res| async move {
            report_controller_reconciled(client, "pod.restarter.commons.stackable.tech", &res)
        })
        .await;
}

async fn reconcile(pod: Arc<Pod>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    tracing::info!("Starting reconciliation ..");
    if pod.metadata.deletion_timestamp.is_some() {
        // Object is already being deleted, no point trying again
        tracing::info!("Pod is already being deleted, taking no action!");
        return Ok(Action::await_change());
    }

    let annotations = &pod.metadata.annotations;

    tracing::debug!("Found expiry annotations: [{:?}]", annotations);

    // Parse timestamp from all found annotations that start with `restarter.stackable.tech/expires-at.`
    // Any error that occurs during parsing of timestamps causes reconciliation to abort here.
    // In case there are multiple annotations on the pod the smallest (soonest) time is returned
    // as result that will be used for evaluation of expiration.
    let pod_expires_at = annotations
        .iter()
        .flatten()
        .filter(|(k, _)| k.starts_with("restarter.stackable.tech/expires-at."))
        .map(|(k, v)| {
            DateTime::parse_from_rfc3339(v).context(UnparseableExpiryTimestampSnafu {
                annotation: k,
                value: v,
            })
        })
        .min_by_key(|res| {
            // Prefer propagating errors over successful cases
            (res.is_ok(), res.as_ref().ok().cloned())
        })
        .transpose()?;

    tracing::debug!(
        "Proceeding with closest expiration time [{:?}]",
        pod_expires_at
    );
    let now = DateTime::<FixedOffset>::from(Utc::now());

    // Calculate the time remaining from now until the stated expiration time by subtraction
    // The call to `chrono::Duration::to_std()` returns an error if the resulting duration is
    // negative -> i.e. when the certificate has expired.

    let time_until_pod_expires = pod_expires_at.map(|expires_at| (expires_at - now).to_std());

    // Match on result of subtraction, possible cases:
    // Some(Error<...>) -> duration was negative, cert has expired
    // Some(Ok<Duration<>>) -> duration was positive, cert still valid
    // None -> there were no annotations to process, pod is not in scope for this code
    match time_until_pod_expires {
        Some(Err(_has_already_expired)) => {
            tracing::info!(
                "Evicting pod, due to stated expiration date being reached - valid_until=[{}]",
                pod_expires_at.unwrap()
            );
            let pods = ctx.client.get_api::<Pod>(
                pod.metadata
                    .namespace
                    .as_deref()
                    .context(PodHasNoNamespaceSnafu)?,
            );
            pods.evict(
                pod.metadata.name.as_deref().context(PodHasNoNameSnafu)?,
                &EvictParams::default(),
            )
            .await
            .context(EvictPodSnafu)?;
            Ok(Action::await_change())
        }
        Some(Ok(time_until_pod_expires)) => {
            tracing::info!(
                "Certificate still valid until [{:?}], reqeueing with delay of [{:?}]",
                pod_expires_at,
                time_until_pod_expires
            );
            Ok(Action::requeue(time_until_pod_expires))
        }
        None => {
            tracing::info!("No expiry annotations found, ignoring pod!");
            Ok(Action::await_change())
        }
    }
}

fn error_policy(_obj: Arc<Pod>, _error: &Error, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(Duration::from_secs(5))
}
