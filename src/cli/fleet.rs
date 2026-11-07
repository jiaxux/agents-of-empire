//! Read-only `aoe fleet` projection.

use anyhow::Result;
use clap::Args;

use crate::fleet::FleetReport;
use crate::session::Storage;

#[derive(Args)]
pub struct FleetArgs {
    /// Emit the versioned shared projection as JSON
    #[arg(long)]
    json: bool,
}

#[tracing::instrument(target = "cli.fleet", skip_all, fields(profile = %profile))]
pub fn run(profile: &str, args: FleetArgs) -> Result<()> {
    let storage = Storage::open_unwatched(profile)?;
    let (mut instances, _) = storage.load_with_groups()?;
    for instance in &mut instances {
        instance.source_profile = storage.profile().to_string();
    }
    let max_workers = crate::session::profile_config::resolve_config_or_warn(profile)
        .acp
        .max_concurrent_workers;
    let report = FleetReport::build(&instances, max_workers);
    if args.json {
        return super::output::print_json(&report);
    }

    println!("Fleet Operations · {} session(s)", report.sessions);
    println!(
        "attention {} · conflicts {} · workers {}/{}",
        report.attention.len(),
        report.conflicts.len(),
        report.budget.active_workers,
        report
            .budget
            .max_active_workers
            .map_or_else(|| "unknown".to_string(), |limit| limit.to_string())
    );
    let ready = report
        .readiness
        .iter()
        .filter(|row| row.verdict == "ready")
        .count();
    let blocked = report
        .readiness
        .iter()
        .filter(|row| row.verdict == "blocked")
        .count();
    println!(
        "readiness {} ready · {} blocked · {} unknown/running",
        ready,
        blocked,
        report.readiness.len().saturating_sub(ready + blocked)
    );
    let campaign = report.campaign.as_ref().map_or_else(
        || "none".to_string(),
        |campaign| {
            format!(
                "{} tasks, {} ready, {} done",
                campaign.total, campaign.ready, campaign.completed
            )
        },
    );
    println!(
        "campaign {campaign} · trials {} · checkpoints {}",
        report.trials.len(),
        report.checkpoints.len()
    );
    if !report.attention.is_empty() {
        println!();
        println!("Attention:");
        for item in &report.attention {
            println!("  {:<10} {:<24} {}", item.reason, item.title, item.summary);
        }
    }
    if !report.conflicts.is_empty() {
        println!();
        println!("Conflicts:");
        for conflict in &report.conflicts {
            println!(
                "  {}/{} · {}",
                conflict.repository,
                conflict.path,
                conflict.sessions.join(", ")
            );
        }
    }
    Ok(())
}
