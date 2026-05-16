use anyhow::{Context, Result};

use crate::cli::{InstanceArgs, InstanceCommand, InstanceRmArgs, InstanceUseArgs};

pub async fn run(args: InstanceArgs) -> Result<()> {
    match args.command {
        InstanceCommand::List => list(),
        InstanceCommand::Use(args) => use_instance(args),
        InstanceCommand::Rm(args) => remove(args),
        InstanceCommand::Show => show(),
    }
}

fn list() -> Result<()> {
    let active = zunel_config::active_instance_name();
    let instances = zunel_config::list_instances()?;
    if instances.is_empty() {
        println!("No instances found yet.");
        return Ok(());
    }
    for instance in instances {
        let marker = if instance == active { " *" } else { "" };
        println!(
            "{instance}\t{}{}",
            zunel_config::resolve_instance_home(&instance)?.display(),
            marker
        );
    }
    Ok(())
}

fn use_instance(args: InstanceUseArgs) -> Result<()> {
    match zunel_config::set_sticky_instance(&args.name) {
        Ok(()) => {}
        Err(err @ zunel_config::Error::InvalidInstanceName(_)) => {
            eprintln!("Error: {err}");
            std::process::exit(2);
        }
        Err(err) => return Err(err.into()),
    }
    if args.name == zunel_config::DEFAULT_INSTANCE_NAME {
        println!("Cleared sticky instance; using the default home.");
    } else {
        println!(
            "Active instance set to {} ({})",
            args.name,
            zunel_config::resolve_instance_home(&args.name)?.display()
        );
    }
    Ok(())
}

fn remove(args: InstanceRmArgs) -> Result<()> {
    if args.name == zunel_config::active_instance_name() {
        anyhow::bail!(
            "Refusing to delete the active instance {:?}. Switch with `zunel instance use default` first.",
            args.name
        );
    }
    if !args.force {
        anyhow::bail!("refusing to remove instance without --force");
    }
    let directory = zunel_config::resolve_instance_home(&args.name)?;
    if !directory.exists() {
        println!(
            "No directory at {}; nothing to remove.",
            directory.display()
        );
        return Ok(());
    }
    std::fs::remove_dir_all(&directory)
        .with_context(|| format!("removing {}", directory.display()))?;
    println!("Removed {}", directory.display());
    Ok(())
}

fn show() -> Result<()> {
    println!("instance: {}", zunel_config::active_instance_name());
    println!("home: {}", zunel_config::active_instance_home()?.display());
    Ok(())
}
