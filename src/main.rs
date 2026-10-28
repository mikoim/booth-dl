use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use booth_dl::client::{BoothClient, Fatal};
use booth_dl::config::{self, SESSION_COOKIE};
use booth_dl::library::Tab;
use booth_dl::{sync, verify};

/// Mirror the digital items in your BOOTH library to a local directory.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Store the BOOTH session cookie
    Login,
    /// Download everything in the library that is not downloaded yet
    Sync {
        /// Output directory
        #[arg(long, default_value = ".")]
        out: PathBuf,
        /// Number of parallel downloads
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=4))]
        jobs: u8,
        /// Re-check downloaded files against the server and refresh sales pages
        #[arg(long)]
        check_updates: bool,
    },
    /// Re-verify downloaded files without network access
    Verify {
        /// Output directory
        #[arg(long, default_value = ".")]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns whether everything succeeded.
fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Login => login().map(|()| true),
        Command::Sync {
            out,
            jobs,
            check_updates,
        } => sync_command(sync::SyncOptions {
            out,
            jobs: jobs.into(),
            check_updates,
        }),
        Command::Verify { out } => verify_command(&out),
    }
}

fn login() -> Result<()> {
    let input =
        rpassword::prompt_password(format!("Paste the value of the {SESSION_COOKIE} cookie: "))?;
    let value = config::normalize_session_input(&input);
    if value.is_empty() {
        bail!("no value entered");
    }
    let client = BoothClient::new(value);
    if let Err(e) = client.get_text(&Tab::Purchases.url(client.accounts_url(), 1)) {
        if matches!(e.downcast_ref::<Fatal>(), Some(Fatal::SessionExpired)) {
            bail!(
                "BOOTH rejected this cookie; copy the current {SESSION_COOKIE} value from your browser and try again"
            );
        }
        return Err(e);
    }
    config::save_session(&client.session())?;
    println!(
        "Logged in. Session saved to {}",
        config::session_path()?.display()
    );
    Ok(())
}

fn sync_command(options: sync::SyncOptions) -> Result<bool> {
    let session = config::load_session()?;
    let client = BoothClient::new(session.value.clone());
    let result = sync::run(&client, &options);
    // BOOTH re-issues the cookie on every response; keep the newest one.
    if !session.from_env && client.session() != session.value {
        config::save_session(&client.session())?;
    }
    let summary = result?;

    println!(
        "Downloaded {}, updated {}, skipped {}, failed {}.",
        summary.downloaded,
        summary.updated,
        summary.skipped,
        summary.failures.len()
    );
    print_list("Warnings", &summary.warnings);
    print_list("Failures", &summary.failures);
    Ok(summary.failures.is_empty())
}

fn verify_command(out: &std::path::Path) -> Result<bool> {
    let summary = verify::run(out)?;
    println!(
        "Checked {} files, {} problems.",
        summary.checked,
        summary.problems.len()
    );
    print_list("Problems", &summary.problems);
    Ok(summary.problems.is_empty())
}

fn print_list(title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    println!("{title}:");
    for line in lines {
        println!("  {line}");
    }
}
