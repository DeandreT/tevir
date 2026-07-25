mod config;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use config::{Config, Role};
use platform::{EnvironmentStatus, PlatformReport};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Doctor { json } => doctor(json),
        Command::Check { config } => check_config(&config),
    }
}

fn doctor(json: bool) -> Result<(), CliError> {
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
    Config(#[from] config::ConfigError),
    #[error("desktop-session prerequisites are not satisfied")]
    PlatformUnavailable,
    #[error("could not encode JSON report: {0}")]
    Json(#[from] serde_json::Error),
}
