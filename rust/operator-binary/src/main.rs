mod restart_controller;

use clap::Parser;
use futures::pin_mut;
use stackable_operator::{
    CustomResourceExt,
    cli::{Command, ProductOperatorRun},
    commons::{
        authentication::AuthenticationClass,
        s3::{S3Bucket, S3Connection},
    },
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
            AuthenticationClass::print_yaml_schema(built_info::PKG_VERSION)?;
            S3Connection::print_yaml_schema(built_info::PKG_VERSION)?;
            S3Bucket::print_yaml_schema(built_info::PKG_VERSION)?;
        }
        Command::Run(ProductOperatorRun {
            product_config: _,
            watch_namespace,
            telemetry_arguments,
            cluster_info_opts,
        }) => {
            // NOTE (@NickLarsenNZ): Before stackable-telemetry was used:
            // - The console log level was set by `COMMONS_OPERATOR_LOG`, and is now `CONSOLE_LOG` (when using Tracing::pre_configured).
            // - The file log level was (maybe?) set by `COMMONS_OPERATOR_LOG`, and is now set via `FILE_LOG` (when using Tracing::pre_configured).
            // - The file log directory was set by `COMMONS_OPERATOR_LOG_DIRECTORY`, and is now set by `ROLLING_LOGS_DIR` (or via `--rolling-logs <DIRECTORY>`).
            let _tracing_guard =
                Tracing::pre_configured(built_info::PKG_NAME, telemetry_arguments).init()?;

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
                &cluster_info_opts,
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
