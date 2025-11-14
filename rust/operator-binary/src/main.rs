// TODO: Look into how to properly resolve `clippy::large_enum_variant`.
// This will need changes in our and upstream error types.
#![allow(clippy::large_enum_variant)]

use anyhow::anyhow;
use clap::Parser;
use futures::{FutureExt, TryFutureExt};
use stackable_operator::{
    YamlSchema as _,
    cli::{Command, RunArguments},
    crd::{
        authentication::core::{AuthenticationClass, AuthenticationClassVersion},
        s3::{S3Bucket, S3BucketVersion, S3Connection, S3ConnectionVersion},
    },
    eos::EndOfSupportChecker,
    shared::yaml::SerializeOptions,
    telemetry::Tracing,
};
use webhooks::create_webhook;

mod restart_controller;
mod webhooks;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub const OPERATOR_NAME: &str = "commons.stackable.tech";
pub const FIELD_MANAGER: &str = "commons-operator";

#[derive(Parser)]
#[clap(about, author)]
struct Opts {
    #[clap(subcommand)]
    cmd: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    match opts.cmd {
        Command::Crd => {
            AuthenticationClass::merged_crd(AuthenticationClassVersion::V1Alpha1)?
                .print_yaml_schema(built_info::PKG_VERSION, SerializeOptions::default())?;
            S3Connection::merged_crd(S3ConnectionVersion::V1Alpha1)?
                .print_yaml_schema(built_info::PKG_VERSION, SerializeOptions::default())?;
            S3Bucket::merged_crd(S3BucketVersion::V1Alpha1)?
                .print_yaml_schema(built_info::PKG_VERSION, SerializeOptions::default())?;
        }
        Command::Run(RunArguments {
            product_config: _,
            watch_namespace,
            operator_environment,
            maintenance,
            common,
        }) => {
            // NOTE (@NickLarsenNZ): Before stackable-telemetry was used:
            // - The console log level was set by `COMMONS_OPERATOR_LOG`, and is now `CONSOLE_LOG` (when using Tracing::pre_configured).
            // - The file log level was (maybe?) set by `COMMONS_OPERATOR_LOG`, and is now set via `FILE_LOG` (when using Tracing::pre_configured).
            // - The file log directory was set by `COMMONS_OPERATOR_LOG_DIRECTORY`, and is now set by `ROLLING_LOGS_DIR` (or via `--rolling-logs <DIRECTORY>`).
            let _tracing_guard =
                Tracing::pre_configured(built_info::PKG_NAME, common.telemetry).init()?;

            tracing::info!(
                built_info.pkg_version = built_info::PKG_VERSION,
                built_info.git_version = built_info::GIT_VERSION,
                built_info.target = built_info::TARGET,
                built_info.built_time_utc = built_info::BUILT_TIME_UTC,
                built_info.rustc_version = built_info::RUSTC_VERSION,
                "Starting {description}",
                description = built_info::PKG_DESCRIPTION
            );

            let eos_checker =
                EndOfSupportChecker::new(built_info::BUILT_TIME_UTC, maintenance.end_of_support)?
                    .run()
                    .map(anyhow::Ok);

            let client = stackable_operator::client::initialize_operator(
                Some("commons.stackable.tech".to_string()),
                &common.cluster_info,
            )
            .await?;

            let webhook = create_webhook(
                &operator_environment,
                // TODO: Make user configurable
                false,
                client.as_kube_client(),
            )
            .await?;
            let webhook = webhook
                .run()
                .map_err(|err| anyhow!(err).context("failed to run webhook"));

            let sts_restart_controller =
                restart_controller::statefulset::start(&client, &watch_namespace).map(anyhow::Ok);
            let pod_restart_controller =
                restart_controller::pod::start(&client, &watch_namespace).map(anyhow::Ok);

            futures::try_join!(
                sts_restart_controller,
                pod_restart_controller,
                eos_checker,
                webhook
            )?;
        }
    }

    Ok(())
}
