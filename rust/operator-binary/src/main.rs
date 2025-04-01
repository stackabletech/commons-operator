mod restart_controller;

use clap::Parser;
use futures::pin_mut;
use stackable_operator::{
    cli::{Command, ProductOperatorRun},
    commons::{
        authentication::AuthenticationClass,
        s3::{S3Bucket, S3Connection},
    },
    CustomResourceExt,
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
            tracing_target,
            cluster_info_opts,
        }) => {
            stackable_operator::logging::initialize_logging(
                "COMMONS_OPERATOR_LOG",
                "commons",
                tracing_target,
            );
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
