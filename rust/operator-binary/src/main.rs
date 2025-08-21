// TODO: Look into how to properly resolve `clippy::large_enum_variant`.
// This will need changes in our and upstream error types.
#![allow(clippy::large_enum_variant)]

mod restart_controller;

use clap::Parser;
use futures::pin_mut;
use stackable_operator::{
    YamlSchema as _,
    cli::{Command, ProductOperatorRun},
    crd::{
        authentication::core::{AuthenticationClass, AuthenticationClassVersion},
        s3::{S3Bucket, S3BucketVersion, S3Connection, S3ConnectionVersion},
    },
    shared::yaml::SerializeOptions,
    telemetry::Tracing,
};

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

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
        Command::Run(ProductOperatorRun {
            product_config: _,
            watch_namespace,
            operator_environment: _,
            telemetry,
            cluster_info,
        }) => {
            // NOTE (@NickLarsenNZ): Before stackable-telemetry was used:
            // - The console log level was set by `COMMONS_OPERATOR_LOG`, and is now `CONSOLE_LOG` (when using Tracing::pre_configured).
            // - The file log level was (maybe?) set by `COMMONS_OPERATOR_LOG`, and is now set via `FILE_LOG` (when using Tracing::pre_configured).
            // - The file log directory was set by `COMMONS_OPERATOR_LOG_DIRECTORY`, and is now set by `ROLLING_LOGS_DIR` (or via `--rolling-logs <DIRECTORY>`).
            let _tracing_guard = Tracing::pre_configured(built_info::PKG_NAME, telemetry).init()?;

            tracing::info!(
                built_info.pkg_version = built_info::PKG_VERSION,
                built_info.git_version = built_info::GIT_VERSION,
                built_info.target = built_info::TARGET,
                built_info.built_time_utc = built_info::BUILT_TIME_UTC,
                built_info.rustc_version = built_info::RUSTC_VERSION,
                "Starting {description}",
                description = built_info::PKG_DESCRIPTION
            );

            let client = stackable_operator::client::initialize_operator(
                Some("commons.stackable.tech".to_string()),
                &cluster_info,
            )
            .await?;

            let sts_restart_controller =
                restart_controller::statefulset::start(&client, &watch_namespace);
            let pod_restart_controller = restart_controller::pod::start(&client, &watch_namespace);
            pin_mut!(sts_restart_controller, pod_restart_controller);
            futures::future::select(sts_restart_controller, pod_restart_controller).await;
        }
    }

    Ok(())
}
