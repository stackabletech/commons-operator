mod pod_enrichment_controller;
mod restart_controller;

use built_info::PKG_VERSION;
use futures::pin_mut;
use stackable_operator::cli::{Command, ProductOperatorRun};

use clap::{crate_description, crate_version, Parser};
use stackable_operator::commons::{
    authentication::AuthenticationClass,
    s3::{S3Bucket, S3Connection},
};
use stackable_operator::CustomResourceExt;

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
            AuthenticationClass::print_yaml_schema(PKG_VERSION)?;
            S3Connection::print_yaml_schema(PKG_VERSION)?;
            S3Bucket::print_yaml_schema(PKG_VERSION)?;
        }
        Command::Run(ProductOperatorRun {
            product_config: _,
            watch_namespace,
            tracing_target,
        }) => {
            stackable_operator::logging::initialize_logging(
                "COMMONS_OPERATOR_LOG",
                "commons",
                tracing_target,
            );
            stackable_operator::utils::print_startup_string(
                crate_description!(),
                crate_version!(),
                built_info::GIT_VERSION,
                built_info::TARGET,
                built_info::BUILT_TIME_UTC,
                built_info::RUSTC_VERSION,
            );

            let client = stackable_operator::client::create_client(Some(
                "commons.stackable.tech".to_string(),
            ))
            .await?;

            let sts_restart_controller =
                restart_controller::statefulset::start(&client, &watch_namespace);
            let pod_restart_controller = restart_controller::pod::start(&client, &watch_namespace);
            let pod_enrichment_controller =
                pod_enrichment_controller::start(&client, &watch_namespace);
            pin_mut!(
                sts_restart_controller,
                pod_restart_controller,
                pod_enrichment_controller
            );
            futures::future::select(
                futures::future::select(sts_restart_controller, pod_restart_controller),
                pod_enrichment_controller,
            )
            .await;
        }
    }

    Ok(())
}
