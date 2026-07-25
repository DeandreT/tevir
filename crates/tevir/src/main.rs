mod app;
mod config;
mod settings;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use config::{Config, Role};
use domain::NodeId;
use platform::{EnvironmentStatus, PlatformReport};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the desktop control surface.
    Ui {
        /// Create or load this node identity.
        #[arg(long)]
        node: Option<String>,
        /// Override the application data directory.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
    /// Inspect native desktop-session prerequisites.
    Doctor {
        /// Emit a machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Validate a Tevir configuration without starting a session.
    Check {
        #[arg(value_name = "PATH")]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let logging = match logging_directory(&cli) {
        Ok(directory) => telemetry::initialize(&directory).or_else(|error| {
            eprintln!("warning: persistent logging unavailable: {error}");
            telemetry::initialize_ephemeral()
        }),
        Err(error) => {
            eprintln!("warning: persistent logging unavailable: {error}");
            telemetry::initialize_ephemeral()
        }
    };
    let logging = match logging {
        Ok(logging) => logging,
        Err(error) => {
            eprintln!("error: could not initialize logging: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "application started");

    match run(cli, logging.buffer()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "application failed");
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn logging_directory(cli: &Cli) -> Result<PathBuf, CliError> {
    let data_directory = match &cli.command {
        Some(Command::Ui {
            data_dir: Some(data_directory),
            ..
        }) => data_directory.clone(),
        None
        | Some(Command::Ui { data_dir: None, .. })
        | Some(Command::Doctor { .. })
        | Some(Command::Check { .. }) => {
            settings::default_data_directory().map_err(app::AppError::from)?
        }
    };
    Ok(data_directory.join("logs"))
}

fn run(cli: Cli, logs: telemetry::LogBuffer) -> Result<(), CliError> {
    match cli.command {
        None => open_ui(None, None, logs),
        Some(Command::Ui { node, data_dir }) => open_ui(node, data_dir, logs),
        Some(Command::Doctor { json }) => doctor(json),
        Some(Command::Check { config }) => check_config(&config),
    }
}

fn open_ui(
    node: Option<String>,
    data_dir: Option<PathBuf>,
    logs: telemetry::LogBuffer,
) -> Result<(), CliError> {
    let node = node.map(NodeId::new).transpose()?;
    let data_directory = data_dir
        .map_or_else(settings::default_data_directory, Ok)
        .map_err(app::AppError::from)?;
    app::run(data_directory, node, logs)?;
    Ok(())
}

fn doctor(json: bool) -> Result<(), CliError> {
    tracing::info!(json, "running platform diagnostics");
    let report = platform::probe_host();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if report.is_available() {
        Ok(())
    } else {
        Err(CliError::PlatformUnavailable)
    }
}

fn print_report(report: &PlatformReport) {
    println!("platform: {:?}", report.platform);
    println!(
        "environment: {}",
        match report.status {
            EnvironmentStatus::Available => "available",
            EnvironmentStatus::Unavailable => "unavailable",
        }
    );
    for issue in &report.issues {
        println!("issue: {issue}");
    }
}

fn check_config(path: &std::path::Path) -> Result<(), CliError> {
    tracing::info!(path = %path.display(), "validating configuration");
    let config = Config::load(path)?;
    match &config.role {
        Role::Controller { listen, topology } => println!(
            "valid controller configuration for `{}`: listening on {listen} with {} screen(s)",
            config.node,
            topology.screens().len()
        ),
        Role::Agent { controller } => println!(
            "valid agent configuration for `{}`: controller at {controller}",
            config.node
        ),
    }
    Ok(())
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    App(#[from] app::AppError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("desktop-session prerequisites are not satisfied")]
    PlatformUnavailable,
    #[error("could not encode JSON report: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Node(#[from] domain::NodeIdError),
}
