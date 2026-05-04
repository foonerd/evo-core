//! `evo-acceptance` CLI: thin clap wrapper around the runner.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use evo_acceptance::{
    inspect_cmd,
    plan::ScenarioPlan,
    report::ReadinessReport,
    runner::{RunOptions, Runner},
    target::TargetDescriptor,
};

#[derive(Parser, Debug)]
#[command(
    name = "evo-acceptance",
    version,
    about = "Adaptive acceptance harness for the evo framework. Runs T1-T5 scenarios against any target and emits a readiness report."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the scenario plan against the target and emit a readiness report.
    Run {
        /// Path to the target descriptor (TOML).
        #[arg(long)]
        target: PathBuf,
        /// Path to the scenario plan (TOML).
        #[arg(long)]
        plan: PathBuf,
        /// Release tag this run covers, e.g. v0.1.12.
        #[arg(long)]
        release: String,
        /// Release-candidate ordinal (1, 2, ...).
        #[arg(long, default_value_t = 1)]
        rc: u32,
        /// Where to write the Markdown readiness report.
        #[arg(long)]
        output: PathBuf,
        /// Optional: also write a structured JSON sidecar of the per-scenario outcomes.
        #[arg(long)]
        output_json: Option<PathBuf>,
    },
    /// Run pre-flight inspectors against the target and emit a Markdown
    /// inspection report (loaded modules, HID tree, i2cdetect, KMS/DRM
    /// connectors, input devices, IIO sensors, boot config, kernel
    /// cmdline, devicetree overlays). Surfaces discrepancies between
    /// declared capabilities and detected hardware/state.
    Inspect {
        /// Path to the target descriptor (TOML).
        #[arg(long)]
        target: PathBuf,
        /// Where to write the Markdown inspection report.
        #[arg(long)]
        output: PathBuf,
        /// Optional: also write a structured JSON sidecar of findings + discrepancies.
        #[arg(long)]
        output_json: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("evo-acceptance: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Cmd::Run {
            target,
            plan,
            release,
            rc,
            output,
            output_json,
        } => {
            let target = TargetDescriptor::from_file(&target)?;
            let plan = ScenarioPlan::from_file(&plan)?;
            let runner = Runner::new(target.clone(), plan.clone());

            let outcomes = runner
                .run(&RunOptions {
                    release: release.clone(),
                    rc,
                })
                .await?;
            let report = ReadinessReport::new(release, rc, &target, outcomes);
            report.write_markdown(&plan, &target, &output)?;
            if let Some(json_path) = output_json {
                report.write_json(&json_path)?;
            }
            println!("evo-acceptance: report written to {}", output.display());
            Ok(())
        }
        Cmd::Inspect {
            target,
            output,
            output_json,
        } => {
            let target = TargetDescriptor::from_file(&target)?;
            let report = inspect_cmd::run_inspect(
                target,
                &output,
                output_json.as_deref(),
            )
            .await?;
            println!(
                "evo-acceptance: inspection report written to {}",
                output.display()
            );
            println!(
                "evo-acceptance: {} discrepancies surfaced.",
                report.discrepancies.len()
            );
            Ok(())
        }
    }
}
