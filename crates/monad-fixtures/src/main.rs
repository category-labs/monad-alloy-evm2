//! Command-line entry point for the Monad blockchain fixture runner.

use clap::Parser;
use eyre::Result;
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

/// Replay EEST blockchain JSON fixtures with Monad execution rules.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// JSON files or directories to search recursively.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Select case names containing this substring.
    #[arg(long)]
    filter: Option<String>,
    /// Write a structured JSON report to stdout.
    #[arg(long)]
    json: bool,
    /// Return a failing exit code if any selected case is unsupported.
    #[arg(long)]
    fail_on_unsupported: bool,
}

fn run(args: Args) -> Result<u8> {
    let report = monad_evm2_fixtures::run(&args.paths, args.filter.as_deref())?;
    let mut output = io::stdout().lock();
    if args.json {
        serde_json::to_writer_pretty(&mut output, &report)?;
        writeln!(output)?;
    } else {
        writeln!(output, "Verification: {}", report.verification)?;
        for case in &report.cases {
            let status = match case.status {
                monad_evm2_fixtures::Status::Passed => "PASS",
                monad_evm2_fixtures::Status::Failed => "FAIL",
                monad_evm2_fixtures::Status::Unsupported => "UNSUPPORTED",
            };
            writeln!(
                output,
                "{status} {} :: {} — {}; {} parity checks, {} differences",
                case.file.display(),
                case.name,
                case.message,
                case.parity.checks,
                case.parity.differences.len(),
            )?;
            for difference in &case.parity.differences {
                writeln!(
                    output,
                    "  DIFFERENCE {}: {}",
                    difference.at,
                    serde_json::to_string(difference)?
                )?;
            }
        }
        writeln!(
            output,
            "{} passed, {} failed, {} unsupported ({} total)",
            report.summary.passed,
            report.summary.failed,
            report.summary.unsupported,
            report.summary.total
        )?;
        writeln!(
            output,
            "{} parity checks, {} differences; {} reference receipts corrected to full-limit charging",
            report.summary.parity_checks,
            report.summary.parity_differences,
            report.summary.corrected_reference_receipts
        )?;
    }
    Ok(report.exit_code(args.fail_on_unsupported))
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "error: {error:#}");
            ExitCode::from(2)
        }
    }
}
