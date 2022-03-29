use stackable_operator::cli::Command;

use clap::Parser;
use stackable_commons_crd::authentication::AuthenticationClass;
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
    stackable_operator::logging::initialize_logging("COMMONS_OPERATOR_LOG");

    let opts = Opts::parse();
    match opts.cmd {
        Command::Crd => println!("{}", serde_yaml::to_string(&AuthenticationClass::crd())?,),
        Command::Run(_) => {
            todo!();
        }
    }

    Ok(())
}
