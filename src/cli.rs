use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use std::path::PathBuf;

use crate::util::VERSION;

#[derive(Parser)]
#[command(name="linkstart", version=VERSION, about="LinkStart v1 Preview 本機 Runtime", disable_help_subcommand=true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}
#[derive(Subcommand)]
pub(crate) enum Command {
    Help(HelpArgs),
    Version(JsonFlag),
    Status(StateArgs),
    Ps(StateArgs),
    Doctor(StateArgs),
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Connections {
        #[command(subcommand)]
        command: ListCommand,
    },
    Apps {
        #[command(subcommand)]
        command: ListCommand,
    },
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    Feedback {
        #[command(subcommand)]
        command: FeedbackCommand,
    },
}
#[derive(Args)]
pub(crate) struct JsonFlag {
    #[arg(long)]
    pub(crate) json: bool,
}
#[derive(Args)]
pub(crate) struct HelpArgs {
    pub(crate) command: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
}
#[derive(Args, Clone)]
pub(crate) struct StateArgs {
    #[arg(long, default_value_os_t=default_state_dir())]
    pub(crate) state_dir: PathBuf,
    #[arg(long)]
    pub(crate) json: bool,
}
#[derive(Subcommand)]
pub(crate) enum DaemonCommand {
    Start(StartArgs),
    Stop(StateArgs),
    Restart(StartArgs),
    #[command(hide = true)]
    Run(StartArgs),
}
#[derive(Args, Clone)]
pub(crate) struct StartArgs {
    #[command(flatten)]
    pub(crate) state: StateArgs,
    #[arg(long, default_value = "127.0.0.1")]
    pub(crate) host: String,
    #[arg(long, default_value_t = 45831)]
    pub(crate) port: u16,
}
#[derive(Deserialize)]
pub(crate) struct DaemonRecord {
    pub(crate) pid: u32,
    pub(crate) address: String,
    pub(crate) version: String,
    #[serde(rename = "protocolMajor")]
    pub(crate) protocol_major: String,
}
#[derive(Subcommand)]
pub(crate) enum ListCommand {
    List(StateArgs),
}
#[derive(Subcommand)]
pub(crate) enum MonitorCommand {
    Wait(MonitorWaitArgs),
    Ack(MonitorAckArgs),
}
#[derive(Args)]
pub(crate) struct MonitorWaitArgs {
    #[command(flatten)]
    pub(crate) state: StateArgs,
    #[arg(long)]
    pub(crate) connection_id: String,
    #[arg(long)]
    pub(crate) capability: String,
    #[arg(long, default_value_t = 30)]
    pub(crate) timeout_seconds: u64,
}
#[derive(Args)]
pub(crate) struct MonitorAckArgs {
    #[command(flatten)]
    pub(crate) state: StateArgs,
    #[arg(long)]
    pub(crate) connection_id: String,
    #[arg(long)]
    pub(crate) capability: String,
    #[arg(long)]
    pub(crate) event_id: String,
}
#[derive(Subcommand)]
pub(crate) enum FeedbackCommand {
    Send(FeedbackSendArgs),
}
#[derive(Args)]
pub(crate) struct FeedbackSendArgs {
    #[command(flatten)]
    pub(crate) state: StateArgs,
    #[arg(long)]
    pub(crate) connection_id: String,
    #[arg(long)]
    pub(crate) capability: String,
    #[arg(long)]
    pub(crate) app_instance_id: String,
    #[arg(long)]
    pub(crate) feedback_id: String,
    #[arg(long)]
    pub(crate) payload: String,
    #[arg(long)]
    pub(crate) in_reply_to_event_id: Option<String>,
}

pub(crate) fn default_state_dir() -> PathBuf {
    std::env::var_os("LINKSTART_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("linkstart"))
}
