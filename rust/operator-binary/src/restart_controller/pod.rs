use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use http::StatusCode;
use snafu::{OptionExt, ResultExt, Snafu};
use stackable_operator::{
    client::Client,
    k8s_openapi::{
        api::core::v1::Pod,
        chrono::{self, DateTime, FixedOffset, Utc},
    },
    kube::{
        self,
        api::{EvictParams, PartialObjectMeta},
        core::{DynamicObject, ErrorResponse},
        runtime::{
            Controller,
            controller::{self, Action},
            events::{Recorder, Reporter},
            reflector::ObjectRef,
            watcher,
        },
    },
    logging::controller::{ReconcilerError, report_controller_reconciled},
    namespace::WatchNamespace,
};
use strum::{EnumDiscriminants, IntoStaticStr};

const FULL_CONTROLLER_NAME: &str = "pod.restarter.commons.stackable.tech";

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
        watch_namespace.get_api::<PartialObjectMeta<Pod>>(client),
        watcher::Config::default(),
    );
    let event_recorder = Arc::new(Recorder::new(
        client.as_kube_client(),
        Reporter {
            controller: FULL_CONTROLLER_NAME.to_string(),
            instance: None,
        },
    ));
    controller
        .run(
            reconcile,
            error_policy,
            Arc::new(Ctx {
                client: client.clone(),
            }),
        )
        // We can let the reporting happen in the background
        .for_each_concurrent(
            16, // concurrency limit
            |result| {
                // The event_recorder needs to be shared across all invocations, so that
                // events are correctly aggregated
                let event_recorder = event_recorder.clone();
                async move { report_result(result, event_recorder).await }
            },
        )
        .await;
}

async fn reconcile(pod: Arc<PartialObjectMeta<Pod>>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    tracing::info!("Starting reconciliation ..");
    if pod.metadata.deletion_timestamp.is_some() {
        // Object is already being deleted, no point trying again
        tracing::info!("Pod is already being deleted, taking no action!");
        return Ok(Action::await_change());
    }

    let annotations = &pod.metadata.annotations;

    tracing::debug!(pod.annotations = ?annotations, "Found expiry annotations");

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
        pod.expires_at = ?pod_expires_at,
        "Proceeding with closest expiration time",
    );
    let now = DateTime::<FixedOffset>::from(Utc::now());

    // Calculate the time remaining from now until the stated expiration time by subtraction
    // The call to `chrono::Duration::to_std()` returns an error if the resulting duration is
    // negative -> i.e. when the pod has expired.
    let time_until_pod_expires = pod_expires_at.map(|expires_at| (expires_at - now).to_std());

    // Match on result of subtraction, possible cases:
    // Some(Error<...>) -> duration was negative, cert has expired
    // Some(Ok<Duration<>>) -> duration was positive, cert still valid
    // None -> there were no annotations to process, pod is not in scope for this code
    match time_until_pod_expires {
        Some(Err(_has_already_expired)) => {
            tracing::info!(
                pod.expires_at = ?pod_expires_at,
                "Evicting pod, due to stated expiration date being reached",
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
            // Clamp the rescheduling delay to a maximum of 6 months to prevent `Action::requeue` from panicking
            // This workaround can be removed once https://github.com/kube-rs/kube/issues/1772 is resolved
            let time_until_pod_expires =
                time_until_pod_expires.min(Duration::from_secs(6 * 30 * 24 * 60 * 60));
            tracing::info!(
                pod.expires_at = ?pod_expires_at,
                recheck_delay = ?time_until_pod_expires,
                "Pod still valid, rescheduling check",
            );
            Ok(Action::requeue(time_until_pod_expires))
        }
        None => {
            tracing::info!("No expiry annotations found, ignoring pod!");
            Ok(Action::await_change())
        }
    }
}

/// Reports the result of reconciliation.
///
/// The Pod restart controller has special handling, as it produced lot's of error messages below.
/// They are expected, as we intentionally use the `Evict` API to restart Pods before e.g. the
/// certificate expires. We roll out PDBs by default. If we try to restart multiple Pods that are
/// part of a PDB, we get this errors.
/// Because of this, we don't emit an error for this case, but only product a INFO trace.
///
/// `ERROR stackable_operator::logging::controller: Failed to reconcile object controller.name="pod.restarter.commons.stackable.tech" error=reconciler for object Pod.v1./trino-worker-default-0.default failed error.sources=[failed to evict Pod, ApiError: Cannot evict pod as it would violate the pod's disruption budget.: TooManyRequests (ErrorResponse { status: "Failure", message: "Cannot evict pod as it would violate the pod's disruption budget.", reason: "TooManyRequests", code: 429 }), Cannot evict pod as it would violate the pod's disruption budget.: TooManyRequests]`
async fn report_result(
    result: Result<
        (ObjectRef<PartialObjectMeta<Pod>>, Action),
        controller::Error<Error, watcher::Error>,
    >,
    event_recorder: Arc<Recorder>,
) {
    if let Err(controller::Error::ReconcilerFailed(
        Error::EvictPod {
            source: evict_pod_error,
        },
        _,
    )) = &result
    {
        const TOO_MANY_REQUESTS_HTTP_CODE: u16 = StatusCode::TOO_MANY_REQUESTS.as_u16();
        if let kube::Error::Api(ErrorResponse {
            code: TOO_MANY_REQUESTS_HTTP_CODE,
            ..
        }) = evict_pod_error
        {
            tracing::info!(
                ?evict_pod_error,
                "Tried to evict Pod, but wasn't allowed to do so, as it would violate the Pod's disruption budget. Retrying later"
            );
        }
    }

    report_controller_reconciled(&event_recorder, FULL_CONTROLLER_NAME, &result).await;
}

fn error_policy(_obj: Arc<PartialObjectMeta<Pod>>, _error: &Error, _ctx: Arc<Ctx>) -> Action {
    Action::requeue(Duration::from_secs(5))
}
