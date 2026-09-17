// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::{self, Debug, Display};
use std::io::{self, Write as _};
use std::marker::PhantomData;

use log::{Level, LevelFilter, Log, Metadata, Record};
use owo_colors::{Color, OwoColorize as _, Style};

struct Logger {
    level: LevelFilter,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level && metadata.target().starts_with("hinata")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        anstream::eprintln!("{}  {}", prefix(record.level()), record.args());
    }

    fn flush(&self) {}
}

fn prefix(level: Level) -> String {
    let style = match level {
        Level::Error => Style::new().red().bold(),
        Level::Warn => Style::new().yellow(),
        Level::Info => Style::new().green(),
        Level::Debug => Style::new().blue(),
        Level::Trace => Style::new().cyan(),
    };
    let name = level.as_str().to_lowercase();
    format!(
        "{}{}",
        "hinata:".dimmed(),
        format_args!("{name:<5}").style(style)
    )
}

/// Asks a yes or no question on the terminal, where anything but yes means no.
pub fn confirm(question: &str) -> io::Result<bool> {
    anstream::eprint!("{}  {question} {} ", prefix(Level::Warn), "[y/N]".dimmed());
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

pub fn init(verbose: bool) -> Result<(), log::SetLoggerError> {
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    log::set_boxed_logger(Box::new(Logger { level }))?;
    log::set_max_level(level);
    Ok(())
}

pub fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!(
        "{} {}",
        count.green(),
        if count == 1 { singular } else { plural }
    )
}

pub trait LogDisplay {
    fn log_display<C: Color>(&self) -> Ticked<&Self, C>;
}

impl<T: ?Sized> LogDisplay for T {
    fn log_display<C: Color>(&self) -> Ticked<&Self, C> {
        Ticked {
            inner: self,
            color: PhantomData,
        }
    }
}

pub struct Ticked<T, C: Color> {
    inner: T,
    color: PhantomData<C>,
}

macro_rules! impl_fmt_trait {
    ($($trait:ident),*) => {
        $(
            impl<T: $trait, C: Color> $trait for Ticked<T, C> {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    let tick = "`".dimmed();
                    write!(f, "{tick}")?;
                    $trait::fmt(&self.inner.fg::<C>(), f)?;
                    write!(f, "{tick}")
                }
            }
        )*
    };
}

impl_fmt_trait!(Display, Debug);
