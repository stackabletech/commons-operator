mod restart_controller;

use stackable_operator::cli::{Command, ProductOperatorRun};

use clap::Parser;
use stackable_operator::commons::authentication::AuthenticationClass;
use stackable_operator::kube::CustomResourceExt;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

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
        Command::Crd => println!("{}", serde_yaml::to_string(&AuthenticationClass::crd())?,),
        Command::Run(ProductOperatorRun {
            product_config: _,
            watch_namespace: _,
            tracing_target,
        }) => {
            stackable_operator::utils::print_startup_string(
                built_info::PKG_DESCRIPTION,
                built_info::PKG_VERSION,
                built_info::GIT_VERSION,
                built_info::TARGET,
                built_info::BUILT_TIME_UTC,
                built_info::RUSTC_VERSION,
            );
            stackable_operator::logging::initialize_logging(
                "COMMONS_OPERATOR_LOG",
                "commons",
                tracing_target,
            );

            let client = stackable_operator::client::create_client(Some(
                "commons.stackable.tech".to_string(),
            ))
            .await?;

            restart_controller::statefulset::start(&client).await?
        }
    }

    Ok(())
}
