//! Standalone long-form rendering. The Python helper owns the audio/video
//! pipeline; no UI project or store job is created by this command.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::process::Command;

use crate::{graph, models::CausalEvent};

#[derive(Debug)]
pub struct RelaxOptions {
    pub minutes: u32,
    pub output: PathBuf,
    pub seed: u64,
    /// An executable path or a name resolved from PATH, never a shell command.
    pub python: PathBuf,
}

impl RelaxOptions {
    pub fn validate(&self) -> Result<()> {
        if !(1..=480).contains(&self.minutes) {
            bail!("relax: minutes must be between 1 and 480");
        }
        if self.output.file_name().is_none() {
            bail!("relax: output must name an MP4 file");
        }
        if self.python.as_os_str().is_empty() {
            bail!("relax: python executable must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct RelaxReport {
    pub output: PathBuf,
    pub verification: PathBuf,
    pub causal_graph: PathBuf,
    pub integrity: graph::Integrity,
}

/// Run from a source checkout: the helper is bundled under scripts/relax.
pub async fn render(options: &RelaxOptions) -> Result<RelaxReport> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    render_with_helper(
        options,
        &repository.join("scripts/relax/create_relax.py"),
        repository,
    )
    .await
}

async fn render_with_helper(
    options: &RelaxOptions,
    helper: &Path,
    repository: &Path,
) -> Result<RelaxReport> {
    options.validate()?;
    if !helper.is_file() {
        bail!(
            "relax: Python helper is missing: {}; run from the complete source checkout",
            helper.display()
        );
    }
    let working_directory =
        std::env::current_dir().context("relax: cannot resolve the current directory")?;
    let output = if options.output.is_absolute() {
        options.output.clone()
    } else {
        working_directory.join(&options.output)
    };
    let python = if options.python.is_absolute() || options.python.components().count() == 1 {
        options.python.clone()
    } else {
        working_directory.join(&options.python)
    };
    let status = Command::new(&python)
        .arg(helper)
        .arg("--minutes")
        .arg(options.minutes.to_string())
        .arg("--output")
        .arg(&output)
        .arg("--seed")
        .arg(options.seed.to_string())
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| {
            format!(
                "relax: could not start Python executable {}",
                options.python.display()
            )
        })?;
    if !status.success() {
        bail!("relax: Python helper failed with {status}");
    }

    let verification = output.with_extension("verification.json");
    let causal_graph = output.with_extension("causal-graph.json");
    let video_metadata = std::fs::metadata(&output)
        .with_context(|| format!("relax: expected video is missing: {}", output.display()))?;
    if !video_metadata.is_file() || video_metadata.len() == 0 {
        bail!("relax: helper did not produce a non-empty video file");
    }
    let _: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&verification)
            .with_context(|| format!("relax: cannot read {}", verification.display()))?,
    )
    .context("relax: invalid verification JSON")?;
    let events: Vec<CausalEvent> = serde_json::from_slice(
        &std::fs::read(&causal_graph)
            .with_context(|| format!("relax: cannot read {}", causal_graph.display()))?,
    )
    .context("relax: invalid causal graph JSON")?;
    if events.is_empty() {
        bail!("relax: causal graph must contain render evidence");
    }
    let integrity = graph::verify(&events);
    if !integrity.valid {
        bail!(
            "relax: causal graph integrity failed: {}",
            integrity.error.as_deref().unwrap_or("unknown error")
        );
    }
    Ok(RelaxReport {
        output,
        verification,
        causal_graph,
        integrity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(minutes: u32) -> RelaxOptions {
        RelaxOptions {
            minutes,
            output: PathBuf::from("output.mp4"),
            seed: 207,
            python: PathBuf::from("python3"),
        }
    }

    #[test]
    fn duration_accepts_boundaries_and_rejects_outside() {
        for minutes in [1, 60, 480] {
            assert!(options(minutes).validate().is_ok());
        }
        for minutes in [0, 481, u32::MAX] {
            assert!(options(minutes).validate().is_err());
        }
    }

    #[tokio::test]
    async fn missing_helper_fails_before_starting_python() {
        let dir = tempfile::tempdir().unwrap();
        let error = render_with_helper(&options(60), &dir.path().join("missing.py"), dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("helper is missing"));
    }

    #[tokio::test]
    async fn unavailable_interpreter_has_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("create_relax.py");
        std::fs::write(&helper, "").unwrap();
        let mut options = options(60);
        options.python = dir.path().join("missing-python");
        let error = render_with_helper(&options, &helper, dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("could not start Python"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_failure_is_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("failing-helper");
        // A tiny interpreter fixture avoids requiring Python/FFmpeg in tests.
        std::fs::write(&helper, "exit 23\n").unwrap();
        let mut options = options(60);
        options.python = PathBuf::from("/bin/sh");
        let error = render_with_helper(&options, &helper, dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Python helper failed"));
        assert!(error.to_string().contains("23"));
    }
}
