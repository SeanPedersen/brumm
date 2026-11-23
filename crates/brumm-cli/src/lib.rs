//! brumm-cli: headless brumm for software, bots and agents (PLAN-AGENTS.md, issue #14).
//!
//! One code path, two transports: every subcommand becomes protocol calls ([`commands`]) that
//! go either to the daemon over its socket ([`remote`], started on demand by
//! [`daemon::spawn`]) or, with `--no-daemon`, to a host inside this process
//! ([`transport::Transport::local`]). Both run the same [`host::methods`].

pub mod cli;
pub mod commands;
pub mod daemon;
pub mod host;
pub mod output;
pub mod paths;
pub mod protocol;
pub mod remote;
pub mod text;
pub mod transport;

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use serde_json::json;

use crate::{
    cli::{Cli, Cmd, DaemonArgs, DaemonCmd},
    daemon::spawn::{self, Target},
    host::HostConfig,
    output::Output,
    protocol::{RpcError, code},
    transport::Transport,
};

/// Exit codes besides 0 (success) and 2 (usage, from clap).
const EXIT_FAILED: u8 = 1;
const EXIT_TIMEOUT: u8 = 3;

pub async fn run(cli: Cli) -> ExitCode {
    let out = Output { json: cli.json };
    match run_inner(cli, out).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if out.json {
                eprintln!("{}", json!({ "error": error }));
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::from(if error.code == code::TIMEOUT { EXIT_TIMEOUT } else { EXIT_FAILED })
        }
    }
}

struct Resolved {
    root: PathBuf,
    socket: PathBuf,
}

fn resolve_paths(cli: &Cli) -> Result<Resolved> {
    let default_root = paths::default_root()?;
    let root = cli.data_dir.clone().unwrap_or_else(|| default_root.clone());
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| paths::socket_path(&root, root == default_root));
    Ok(Resolved { root, socket })
}

async fn run_inner(cli: Cli, out: Output) -> Result<(), RpcError> {
    let Resolved { root, socket } = resolve_paths(&cli)?;
    let relays = (!cli.relays.is_empty()).then(|| cli.relays.clone());
    let host_config = |max_profiles| HostConfig {
        root: root.clone(),
        relays: relays.clone(),
        max_profiles,
        retry_interval: cli.retry_interval,
    };
    let target = |idle_exit| Target {
        root: root.clone(),
        socket: socket.clone(),
        relays: cli.relays.clone(),
        retry_interval: cli.retry_interval,
        idle_exit,
    };
    if let Cmd::Daemon(DaemonArgs {
        action,
        idle_exit,
        max_profiles,
    }) = &cli.command
    {
        return daemon_command(
            action.unwrap_or(DaemonCmd::Run),
            host_config(*max_profiles),
            target(*idle_exit),
            out,
        )
        .await;
    }
    let transport = if cli.no_daemon {
        Transport::local(host_config(1))
    } else {
        Transport::Remote(spawn::ensure(&target(Some(spawn::AUTO_IDLE_EXIT))).await?)
    };
    let cx = commands::Context {
        transport: &transport,
        profile: &cli.profile,
        out,
    };
    let result = commands::run(&cx, cli.command).await;
    transport.close().await;
    result
}

async fn daemon_command(
    action: DaemonCmd,
    host: HostConfig,
    target: Target,
    out: Output,
) -> Result<(), RpcError> {
    match action {
        DaemonCmd::Run => {
            daemon::run(daemon::DaemonOptions {
                host,
                socket: target.socket,
                idle_exit: target.idle_exit,
            })
            .await?;
        }
        DaemonCmd::Start => {
            let target = Target {
                idle_exit: target.idle_exit.or(Some(spawn::AUTO_IDLE_EXIT)),
                ..target
            };
            let remote = spawn::ensure(&target).await?;
            let status = remote.call("daemon.status", json!({})).await?;
            print_status(out, &status, &target.socket);
        }
        DaemonCmd::Stop => match spawn::connect(&target).await? {
            Some(remote) => {
                remote.call("daemon.stop", json!({})).await?;
                out.emit(&json!({ "stopped": true }), |_| "stopping (draining open profiles)".into());
            }
            None => out.emit(&json!({ "stopped": false }), |_| "no daemon is running".into()),
        },
        DaemonCmd::Status => match spawn::connect(&target).await? {
            Some(remote) => {
                let status = remote.call("daemon.status", json!({})).await?;
                print_status(out, &status, &target.socket);
            }
            None => return Err(RpcError::not_found("no daemon is running")),
        },
    }
    Ok(())
}

fn print_status(out: Output, status: &serde_json::Value, socket: &std::path::Path) {
    let mut status = status.clone();
    status["socket"] = json!(socket);
    out.emit(&status, |s| {
        format!(
            "daemon pid {} (protocol v{}), {} connections, open: {}\nsocket {}",
            s["pid"], s["protocol"], s["connections"], s["open"], socket.display()
        )
    });
}
