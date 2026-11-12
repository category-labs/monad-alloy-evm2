//! Replay blockchain fixtures and report execution checks separately from unsupported cases.

use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};
use walkdir::WalkDir;

mod engine;
mod execute;
mod fixture;
mod parity;
mod state;
pub use parity::{Observation, ParityDifference, ParityReport};

#[cfg(feature = "compare-monad-revm")]
mod reference;

/// Result of checking a single fixture case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// All supported execution assertions matched.
    Passed,
    /// Input was malformed, execution failed unexpectedly, or an assertion differed.
    Failed,
    /// The case requires behavior outside the runner's supported scope.
    Unsupported,
}

/// An individually named case, including its source and diagnostic.
#[derive(Debug, Serialize)]
pub struct CaseReport {
    /// Source JSON file.
    pub file: PathBuf,
    /// Case name from the top-level JSON object, or `<file>` for a file error.
    pub name: String,
    /// Requested fixture network, when available.
    pub network: Option<String>,
    /// Outcome of the case.
    pub status: Status,
    /// Checked block/transaction counts or a specific failure/unsupported reason.
    pub message: String,
    /// Optional comparisons against monad-revm 0.8.0, separate from fixture assertions.
    pub parity: ParityReport,
}

/// Counts of selected cases, with no implicit skips.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// Total selected cases, including file errors.
    pub total: usize,
    /// Cases whose execution assertions matched.
    pub passed: usize,
    /// Cases or files that failed.
    pub failed: usize,
    /// Cases that could not be checked.
    pub unsupported: usize,
    /// Number of operations compared with the reference.
    pub parity_checks: usize,
    /// Number of operations that differ from the reference.
    pub parity_differences: usize,
    /// Reference receipts corrected to charge the full gas limit.
    pub corrected_reference_receipts: usize,
}

/// A deterministic report for a collection of fixture files.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Version of the machine-readable report format.
    pub schema_version: u32,
    /// Scope of a pass; the runner is not a complete consensus validator.
    pub verification: &'static str,
    /// Aggregate results.
    pub summary: Summary,
    /// Individual results in file/name order.
    pub cases: Vec<CaseReport>,
}

impl Report {
    /// Returns 1 for fixture failures or reference differences, 2 when nothing
    /// was executable, and 0 otherwise.
    /// With `fail_on_unsupported`, unsupported cases also return 1.
    pub const fn exit_code(&self, fail_on_unsupported: bool) -> u8 {
        if self.summary.failed > 0 || self.summary.parity_differences > 0 {
            1
        } else if self.summary.passed == 0 {
            2
        } else if fail_on_unsupported && self.summary.unsupported > 0 {
            1
        } else {
            0
        }
    }
}

/// Recursively reads JSON files and executes each matching blockchain case.
///
/// Paths are canonicalized and deduplicated. Directory traversal does not follow
/// symlinks. Corpus `index.json` manifests are excluded from directory discovery.
/// The optional filter is a substring of the case name. Malformed files remain
/// failures even when filtering, and do not prevent other files from running.
/// The `compare-monad-revm` feature additionally compares execution with the reference.
pub fn run(paths: &[PathBuf], filter: Option<&str>) -> Result<Report> {
    let mut files = Vec::new();
    for path in paths {
        let metadata =
            fs::metadata(path).wrap_err_with(|| format!("reading {}", path.display()))?;
        if metadata.is_file() {
            files.push(fs::canonicalize(path)?);
        } else if metadata.is_dir() {
            for entry in WalkDir::new(path).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_file()
                    && entry.path().extension().is_some_and(|ext| ext == "json")
                    && entry.file_name() != "index.json"
                {
                    files.push(fs::canonicalize(entry.path())?);
                }
            }
        } else {
            return Err(eyre!("{} is not a file or directory", path.display()));
        }
    }
    files.sort();
    files.dedup();

    let mut cases = Vec::new();
    for file in files {
        let loaded = fs::read(&file).map_err(eyre::Report::from).and_then(|bytes| {
            serde_json::from_slice::<BTreeMap<String, Value>>(&bytes).map_err(Into::into)
        });
        let entries = match loaded {
            Ok(entries) => entries,
            Err(error) => {
                cases.push(CaseReport {
                    file,
                    name: "<file>".into(),
                    network: None,
                    status: Status::Failed,
                    message: format!("cannot read fixture object: {error:#}"),
                    parity: ParityReport::default(),
                });
                continue;
            }
        };
        for (name, value) in entries {
            if filter.is_some_and(|pattern| !name.contains(pattern)) {
                continue;
            }
            let network = fixture::network_name(&value).map(str::to_owned);
            let mut parity = ParityReport::default();
            let result = fixture::Fixture::parse(&value).and_then(|fixture| {
                execute::execute(&fixture, &mut parity).map_err(fixture::FixtureError::Failed)
            });
            let (status, message) = match result {
                Ok(stats) => (
                    Status::Passed,
                    format!(
                        "{} blocks, {} transactions; execution commitments matched",
                        stats.blocks, stats.transactions
                    ),
                ),
                Err(fixture::FixtureError::Unsupported(reason)) => (Status::Unsupported, reason),
                Err(fixture::FixtureError::Failed(error)) => (Status::Failed, format!("{error:#}")),
            };
            cases.push(CaseReport { file: file.clone(), name, network, status, message, parity });
        }
    }

    let mut summary = Summary { total: cases.len(), ..Default::default() };
    for case in &cases {
        summary.parity_checks += case.parity.checks;
        summary.parity_differences += case.parity.differences.len();
        summary.corrected_reference_receipts += case.parity.corrected_receipts;
        match case.status {
            Status::Passed => summary.passed += 1,
            Status::Failed => summary.failed += 1,
            Status::Unsupported => summary.unsupported += 1,
        }
    }
    Ok(Report {
        schema_version: 1,
        verification: if cfg!(feature = "compare-monad-revm") {
            "valid_block_execution_and_reference_parity"
        } else {
            "valid_block_execution"
        },
        summary,
        cases,
    })
}
