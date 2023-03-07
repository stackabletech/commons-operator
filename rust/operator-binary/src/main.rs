mod pod_enrichment_controller;
mod restart_controller;
mod stackable_cluster_controller;

use futures::pin_mut;
use stackable_operator::{cli::Command, logging::TracingTarget};

use clap::Parser;
use stackable_cluster_controller::crd::StackableCluster;
use stackable_cluster_controller::secret_operator::crd::SecretClass;
use stackable_operator::commons::{
    authentication::AuthenticationClass,
    s3::{S3Bucket, S3Connection},
};
use stackable_operator::CustomResourceExt;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Parser)]
#[clap(about = built_info::PKG_DESCRIPTION, author = stackable_operator::cli::AUTHOR)]
struct Opts {
    #[clap(subcommand)]
    cmd: Command<CommonsOperatorRun>,
}

#[derive(clap::Parser)]
struct CommonsOperatorRun {
    #[clap(long, env)]
    namespace: String,
    /// Tracing log collector system
    #[arg(long, env, default_value_t, value_enum)]
    pub tracing_target: TracingTarget,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    match opts.cmd {
        Command::Crd => {
            StackableCluster::print_yaml_schema()?;
            AuthenticationClass::print_yaml_schema()?;
            S3Connection::print_yaml_schema()?;
            S3Bucket::print_yaml_schema()?;
            SecretClass::print_yaml_schema()?;
        }
        Command::Run(CommonsOperatorRun {
            namespace,
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

            let stackable_cluster_controller =
                stackable_cluster_controller::start(&client, namespace);
            let sts_restart_controller = restart_controller::statefulset::start(&client);
            let pod_restart_controller = restart_controller::pod::start(&client);
            let pod_enrichment_controller = pod_enrichment_controller::start(&client);
            pin_mut!(
                stackable_cluster_controller,
                sts_restart_controller,
                pod_restart_controller,
                pod_enrichment_controller,
            );
            futures::future::select(
                futures::future::select(
                    futures::future::select(stackable_cluster_controller, sts_restart_controller),
                    pod_restart_controller,
                ),
                pod_enrichment_controller,
            )
            .await;
        }
    }

    Ok(())
}
