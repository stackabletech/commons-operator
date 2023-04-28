mod pod_enrichment_controller;
mod restart_controller;

use std::time::Duration;

use futures::pin_mut;
use jemallocator::Jemalloc;
use stackable_operator::cli::{Command, ProductOperatorRun};

use clap::Parser;
use stackable_operator::commons::{
    authentication::AuthenticationClass,
    s3::{S3Bucket, S3Connection},
};
use stackable_operator::CustomResourceExt;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Parser)]
#[clap(about = built_info::PKG_DESCRIPTION, author = stackable_operator::cli::AUTHOR)]
struct Opts {
    #[clap(subcommand)]
    cmd: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    match opts.cmd {
        Command::Crd => {
            AuthenticationClass::print_yaml_schema()?;
            S3Connection::print_yaml_schema()?;
            S3Bucket::print_yaml_schema()?;
        }
        Command::Run(ProductOperatorRun {
            product_config: _,
            watch_namespace: _,
            tracing_target,
        }) => {
            stackable_operator::logging::initialize_logging(
                "COMMONS_OPERATOR_LOG",
                "commons",
                tracing_target,
            );
            stackable_operator::utils::print_startup_string(
                built_info::PKG_DESCRIPTION,
                built_info::PKG_VERSION,
                built_info::GIT_VERSION,
                built_info::TARGET,
                built_info::BUILT_TIME_UTC,
                built_info::RUSTC_VERSION,
            );

            let client = stackable_operator::client::create_client(Some(
                "commons.stackable.tech".to_string(),
            ))
            .await?;

            std::thread::spawn(|| loop {
                jemalloc_ctl::epoch::advance().unwrap();
                println!(
                    "retained: {retained}",
                    retained = jemalloc_ctl::stats::retained::read().unwrap()
                );
                std::thread::sleep(Duration::from_secs(1));
            });

            // let sts_restart_controller = restart_controller::statefulset::start(&client);
            let pod_restart_controller = restart_controller::pod::start(&client);
            // let pod_enrichment_controller = pod_enrichment_controller::start(&client);
            pin_mut!(
                // sts_restart_controller,
                pod_restart_controller,
                // pod_enrichment_controller
            );
            futures::future::select(
                futures::future::select(
                    // sts_restart_controller,
                    futures::future::pending::<()>(),
                    pod_restart_controller,
                ),
                // pod_enrichment_controller,
                futures::future::pending::<()>(),
            )
            .await;
        }
    }

    Ok(())
}
