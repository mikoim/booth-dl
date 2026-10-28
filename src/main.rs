use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use booth_dl::browser::{self, Browser};
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
    Login {
        /// Read the cookie from this browser instead of asking for it
        #[arg(long, value_enum)]
        from_browser: Option<Browser>,
        /// Browser profile to read: a Firefox profile name or directory, or a
        /// Chrome-family profile directory such as "Default"
        #[arg(long, requires = "from_browser")]
        profile: Option<String>,
    },
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
    /// Print a shell completion script
    Completions {
        /// Shell to generate the script for
        #[arg(value_enum)]
        shell: Shell,
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
        Command::Login {
            from_browser,
            profile,
        } => login(from_browser, profile.as_deref()).map(|()| true),
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
        Command::Completions { shell } => {
            write_completions(shell, &mut io::stdout());
            Ok(true)
        }
    }
}

fn login(from_browser: Option<Browser>, profile: Option<&str>) -> Result<()> {
    let value = match from_browser {
        Some(browser) => {
            let found = browser::find_session(browser, profile)?;
            println!(
                "Found the BOOTH session in {browser} profile \"{}\".",
                found.profile
            );
            found.value
        }
        None => {
            let input = rpassword::prompt_password(format!(
                "Paste the value of the {SESSION_COOKIE} cookie: "
            ))?;
            config::normalize_session_input(&input)
        }
    };
    if value.is_empty() {
        bail!("no value entered");
    }
    let client = BoothClient::new(value);
    if let Err(e) = client.get_text(&Tab::Purchases.url(client.accounts_url(), 1)) {
        if matches!(e.downcast_ref::<Fatal>(), Some(Fatal::SessionExpired)) {
            bail!(
                "BOOTH rejected this cookie; log in to BOOTH in your browser and try again \
                 (or paste the current {SESSION_COOKIE} value with `booth-dl login`)"
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

/// Writes the completion script for `shell`, generated from the CLI definition.
fn write_completions(shell: Shell, out: &mut dyn io::Write) {
    clap_complete::generate(shell, &mut Cli::command(), "booth-dl", out);
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

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_the_completions_command() {
        let cli = Cli::try_parse_from(["booth-dl", "completions", "powershell"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions {
                shell: Shell::PowerShell
            }
        ));
    }

    #[test]
    fn completion_scripts_cover_every_command_and_option() {
        for shell in Shell::value_variants() {
            let mut script = Vec::new();
            write_completions(*shell, &mut script);
            let script = String::from_utf8(script).unwrap();
            // Long options without dashes: fish spells them `-l from-browser`.
            for word in [
                "login",
                "sync",
                "verify",
                "completions",
                "from-browser",
                "check-updates",
            ] {
                assert!(script.contains(word), "{shell} script lacks {word}");
            }
        }
    }

    /// clap_complete 4.6.1 and later name the bash `case` labels differently from
    /// the `cmd` values for a hyphenated binary name, which breaks completion after
    /// a subcommand (`booth-dl sync --<TAB>`).
    #[test]
    fn bash_script_has_a_case_for_every_command() {
        let mut script = Vec::new();
        write_completions(Shell::Bash, &mut script);
        let script = String::from_utf8(script).unwrap();
        let labels: Vec<&str> = script
            .lines()
            .filter_map(|line| line.trim().strip_suffix(')'))
            .collect();
        let commands = script.lines().filter_map(|line| {
            line.trim()
                .strip_prefix("cmd=\"")
                .and_then(|rest| rest.strip_suffix('"'))
                .filter(|name| !name.is_empty())
        });
        for command in commands {
            assert!(labels.contains(&command), "no bash case for {command}");
        }
    }
}
