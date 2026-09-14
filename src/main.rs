// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

mod edit;
mod gc;
mod impure;
mod install;
mod link;
mod lock;
mod logging;
mod manifest;
mod nix;
mod pnpm;
mod push;
mod registry;
mod resolve;
mod run;

use std::path::PathBuf;
use std::process::ExitStatus;

use clap::{Parser, Subcommand};
use eyre::Result;

use crate::install::{Lockfile, Update};
use crate::manifest::Group;

#[derive(Parser)]
#[command(version, about = "A JavaScript package manager built on Nix")]
struct Cli {
    /// Project directory
    #[arg(long, global = true, default_value = ".")]
    dir: PathBuf,
    /// Show debug logs
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Resolve dependencies, build `node_modules` with Nix, and link it into the project
    #[command(alias = "i")]
    Install {
        /// Skip devDependencies
        #[arg(long)]
        prod: bool,
        /// Build even if nothing changed since the last install
        #[arg(long)]
        refresh: bool,
        /// Write hinata.lock when installing from pnpm-lock.yaml
        #[arg(long)]
        save_lock: bool,
        /// Fail instead of resolving dependencies or changing hinata.lock
        #[arg(long, conflicts_with = "save_lock")]
        frozen_lockfile: bool,
    },
    /// Add dependencies to package.json and install them
    Add {
        /// Packages, as name, name@range, name@tag or alias@npm:name@range
        #[arg(required = true)]
        packages: Vec<String>,
        /// Save to devDependencies
        #[arg(short = 'D', long, conflicts_with = "save_optional")]
        save_dev: bool,
        /// Save to optionalDependencies
        #[arg(short = 'O', long)]
        save_optional: bool,
        /// Save the exact version rather than a ^ range
        #[arg(short = 'E', long)]
        save_exact: bool,
    },
    /// Remove dependencies from package.json and uninstall them
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Update dependencies to the newest versions their ranges allow (all of them by default)
    #[command(alias = "up")]
    Update { packages: Vec<String> },
    /// Run a package.json script
    Run {
        script: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command with `node_modules/.bin` on PATH
    Exec {
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Push packages built by install scripts, with their dependencies, to a Nix binary cache
    Push {
        /// Nix store URI, such as `s3://bucket` or `file:///srv/cache`
        #[arg(required_unless_present = "print")]
        store: Option<String>,
        /// Print the store paths instead, for tools such as cachix or attic
        #[arg(long, conflicts_with = "store")]
        print: bool,
    },
    /// Remove GC roots of projects that no longer exist and clear cached registry metadata
    Gc,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    logging::init(cli.verbose)?;
    let install = |update| install::Options {
        dir: cli.dir.clone(),
        dev: true,
        refresh: false,
        update,
        lockfile: Lockfile::Save,
    };

    match cli.command {
        Command::Install {
            prod,
            refresh,
            save_lock,
            frozen_lockfile,
        } => install::run(&install::Options {
            dev: !prod,
            refresh,
            lockfile: match (save_lock, frozen_lockfile) {
                (_, true) => Lockfile::Frozen,
                (true, _) => Lockfile::Save,
                _ => Lockfile::Default,
            },
            ..install(Update::Keep)
        })?,
        Command::Add {
            packages,
            save_dev,
            save_optional,
            save_exact,
        } => {
            let group = match (save_dev, save_optional) {
                (true, _) => Group::Dev,
                (_, true) => Group::Optional,
                _ => Group::Prod,
            };
            edit::add(&cli.dir, &packages, group, save_exact)?;
        }
        Command::Remove { packages } => edit::remove(&cli.dir, &packages)?,
        Command::Update { packages } => {
            let update = if packages.is_empty() {
                Update::All
            } else {
                Update::Only(packages.into_iter().collect())
            };
            install::run(&install(update))?;
        }
        Command::Run { script, args } => exit_with(run::script(&cli.dir, &script, &args)?),
        Command::Exec { program, args } => exit_with(run::exec(&cli.dir, &program, &args)?),
        Command::Push { store, .. } => push::run(&cli.dir, store.as_deref())?,
        Command::Gc => gc::run()?,
    }
    Ok(())
}

fn exit_with(status: ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1))
}
