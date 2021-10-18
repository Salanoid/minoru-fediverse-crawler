use crate::ipc;
use anyhow::{anyhow, bail, Context};
use rusqlite::Connection;
use slog::{error, Logger};
use std::env;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use url::Host;

use crate::orchestrator::db;

pub struct CheckerHandle {
    conn: Connection,
    logger: Logger,
    instance: Host,
}

impl CheckerHandle {
    pub fn new(logger: Logger, instance: Host) -> anyhow::Result<Self> {
        let conn = db::open()?;
        db::start_checking(&conn, &instance)?;
        Ok(CheckerHandle {
            conn,
            logger,
            instance,
        })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        check(&self.logger, &self.instance)
            .with_context(|| format!("Failed to check {}", self.instance.to_string()))
    }
}

impl Drop for CheckerHandle {
    fn drop(&mut self) {
        if let Err(e) = db::finish_checking(&self.conn, &self.instance) {
            error!(
                self.logger,
                "Error marking {} as checked in the DB: {}",
                self.instance.to_string(),
                e
            );
        }
    }
}

fn check(logger: &Logger, target: &Host) -> anyhow::Result<()> {
    if let Err(e) = run_checker(logger, target) {
        db::open()
            .and_then(|mut conn| db::reschedule(&mut conn, target))
            .with_context(|| format!("While handling a checker error: {}", e))?;
    }
    Ok(())
}

fn run_checker(logger: &Logger, target: &Host) -> anyhow::Result<()> {
    let exe_path = env::args_os()
        .next()
        .ok_or_else(|| anyhow!("Failed to determine the path to the executable"))?;

    let mut checker = Command::new(exe_path)
        .arg("--check")
        .arg(target.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn a checker")?;
    let result = process_checker_response(logger, target, &mut checker);

    if checker.try_wait().is_err() {
        if let Err(e) = checker.kill() {
            error!(logger, "Failed to kill the checker for {}: {}", target, e);
        }
        if let Err(e) = checker.try_wait() {
            error!(
                logger,
                "The checker for {} survived the kill() somehow: {}", target, e
            );
        }
    }

    result
}

fn process_checker_response(
    logger: &Logger,
    target: &Host,
    checker: &mut Child,
) -> anyhow::Result<()> {
    let output = checker
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to connect to checker's stdout"))?;
    let reader = BufReader::new(output);
    let mut lines = reader.lines();

    let mut conn = db::open()?;

    let state = {
        if let Some(line) = lines.next() {
            let line = line.context("Failed to read a line of checker's response")?;
            serde_json::from_str(&line).context("Failed to deserialize checker's response")?
        } else {
            return db::mark_dead(&mut conn, target);
        }
    };

    match state {
        ipc::CheckerResponse::Peer { peer: _ } => {
            db::mark_dead(&mut conn, target)?;
            bail!("Expected the checker to respond with State, but it responded with Peer");
        }
        ipc::CheckerResponse::State { state } => match state {
            ipc::InstanceState::Alive => {
                db::mark_alive(&mut conn, target)?;
                process_peers(logger, &mut conn, target, lines)?;
            }
            ipc::InstanceState::Moving { to } => {
                println!("{} is moving to {}", target, to);
                db::reschedule(&mut conn, target)?;
            }
            ipc::InstanceState::Moved { to } => {
                println!("{} has moved to {}", target, to);
                db::mark_moved(&mut conn, target, &to)?;
            }
        },
    }

    Ok(())
}

fn process_peers(
    _logger: &Logger,
    conn: &mut Connection,
    target: &Host,
    lines: impl Iterator<Item = std::io::Result<String>>,
) -> anyhow::Result<()> {
    let mut peers_count = 0;
    for response in lines {
        let response = response.context("Failed to read a line of checker's response")?;

        let response: ipc::CheckerResponse =
            serde_json::from_str(&response).context("Failed to deserialize checker's response")?;

        match response {
            ipc::CheckerResponse::State { state: _ } => {
                bail!("Expected the checker to respond with Peer, but it responded with State")
            }
            ipc::CheckerResponse::Peer { peer } => {
                db::add_instance(conn, target, &peer)?;
                peers_count += 1;
            }
        }
    }

    println!("{} has {} peers", target, peers_count);

    Ok(())
}
