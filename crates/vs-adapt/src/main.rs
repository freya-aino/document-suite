//! vs-adapt CLI. See `SPEC.md`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use vs_adapt::{select, Objective, PageEval};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Expand templates into validated candidate DAGs.
    Generate { #[arg(long)] search_space: PathBuf, #[arg(long)] out: PathBuf },
    /// Execute candidates on a dataset (stage cluster).
    Run { #[arg(long)] manifest: PathBuf, #[arg(long)] candidates: PathBuf, #[arg(long)] out: PathBuf },
    /// Compute PageEval records from runs and ground truth / judge.
    Score { #[arg(long)] runs: PathBuf, #[arg(long)] out: PathBuf },
    /// Pick DAGs per class and write the DAG list.
    Select {
        #[arg(long)] evals: PathBuf,
        #[arg(long)] candidates: PathBuf,
        #[arg(long)] version: String,
        #[arg(long)] objective: Option<PathBuf>,
        #[arg(long)] out: PathBuf,
    },
    /// Upload a DAG list as candidate to the API.
    Promote { #[arg(long)] dag_list: PathBuf, #[arg(long)] api: String },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Select { evals, candidates, version, objective, out } => {
            let evals: Vec<PageEval> = std::fs::read_to_string(&evals)?
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            let dags: Vec<vs_dag::Dag> = serde_json::from_str(&std::fs::read_to_string(&candidates)?)?;
            let obj: Objective = match objective {
                Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
                None => Objective::default(),
            };
            let list = select(&evals, dags, &obj, &version, Some(format!("vs-adapt select {}", evals.len())))
                .ok_or_else(|| anyhow::anyhow!("no DAG reaches the quality floor"))?;
            std::fs::write(&out, serde_json::to_string_pretty(&list)?)?;
            println!("wrote {} with {} rules", out.display(), list.routing.rules.len());
            Ok(())
        }
        Cmd::Generate { .. } => anyhow::bail!("generate: not implemented (see SPEC.md 'Candidate generation')"),
        Cmd::Run { .. } => anyhow::bail!("run: not implemented (successive halving on stage cluster)"),
        Cmd::Score { .. } => anyhow::bail!("score: not implemented (metrics table in SPEC.md)"),
        Cmd::Promote { .. } => anyhow::bail!("promote: not implemented (POST /v1/dag-lists)"),
    }
}
