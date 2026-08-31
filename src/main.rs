mod cli;
mod daemon;
mod help;
mod server;
mod store;
mod util;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};

use crate::cli::{
    Cli, Command, DaemonCommand, FeedbackCommand, HelpArgs, ListCommand, MonitorCommand, StartArgs,
};
use crate::daemon::{daemon_control, start_daemon, stop_daemon};
use crate::help::{main_help_json, print_human_help};
use crate::server::serve;
use crate::store::Store;
use crate::util::{canonical, json_line, output, PROTOCOL, VERSION};

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    {
        // Windows 主控台預設 codepage（如 cp950）會把 UTF-8 中文輸出顯示成亂碼。
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleOutputCP(code_page: u32) -> i32;
        }
        unsafe { SetConsoleOutputCP(65001) };
    }
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Help(HelpArgs {
        command: None,
        json: false,
    })) {
        Command::Help(h) => {
            if h.json {
                json_line(main_help_json())
            } else {
                print_human_help(h.command.as_deref());
            }
        }
        Command::Version(f) => output(
            f.json,
            json!({"version":VERSION,"protocolMajor":PROTOCOL,"channel":"LinkStart v1 Preview：Stable core + Preview platform adapters"}),
            VERSION,
        ),
        Command::Status(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.status()?, "daemon 狀態已載入");
        }
        Command::Ps(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.ps()?, "無 secrets 的 connection/app 摘要");
        }
        Command::Doctor(a) => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.doctor()?, "SQLite WAL 與狀態目錄檢查完成");
        }
        Command::Connections {
            command: ListCommand::List(a),
        } => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.connections()?, "connections 清單");
        }
        Command::Apps {
            command: ListCommand::List(a),
        } => {
            let s = Store::open(&a.state_dir)?;
            output(a.json, s.apps()?, "apps 清單");
        }
        Command::Monitor {
            command: MonitorCommand::Wait(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            let until =
                std::time::Instant::now() + std::time::Duration::from_secs(a.timeout_seconds);
            loop {
                if let Some(event) = s.next_received(&a.connection_id)? {
                    output(
                        a.state.json,
                        event,
                        "收到一個 durable App Event；請以 monitor ack 明確接受",
                    );
                    break;
                }
                if std::time::Instant::now() >= until {
                    output(
                        a.state.json,
                        json!({"status":"timeout"}),
                        "等待逾時，請重新 arm monitor wait",
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        Command::Monitor {
            command: MonitorCommand::Ack(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            output(
                a.state.json,
                s.ack_event(&a.connection_id, &a.event_id)?,
                "已記錄 Delivery Ack；不代表模型完成或工具核准",
            );
        }
        Command::Feedback {
            command: FeedbackCommand::Send(a),
        } => {
            let s = Store::open(&a.state.state_dir)?;
            if !s.auth_connection(&a.connection_id, &a.capability)? {
                bail!("capability_invalid")
            }
            let payload = canonical(
                serde_json::from_str(&a.payload).context("feedback payload must be JSON")?,
            );
            output(
                a.state.json,
                s.insert_feedback(
                    &a.connection_id,
                    &a.app_instance_id,
                    &a.feedback_id,
                    a.in_reply_to_event_id.as_deref(),
                    payload,
                )?,
                "Agent Feedback 已 durable 寫入",
            );
        }
        Command::Daemon {
            command: DaemonCommand::Start(a),
        } => {
            daemon_control(&a, start_daemon(&a), "daemon 已啟動或重用")?;
        }
        Command::Daemon {
            command: DaemonCommand::Restart(a),
        } => {
            let result = (|| -> Result<Value> {
                let stopped = stop_daemon(&a.state.state_dir)?;
                let mut result = start_daemon(&a)?;
                result["stoppedBeforeRestart"] = json!(stopped);
                Ok(result)
            })();
            daemon_control(&a, result, "daemon 已重啟")?;
        }
        Command::Daemon {
            command: DaemonCommand::Run(a),
        } => serve(a).await?,
        Command::Daemon {
            command: DaemonCommand::Stop(a),
        } => {
            let result = stop_daemon(&a.state_dir).map(|stopped| json!({"stopped":stopped}));
            let start = StartArgs {
                state: a,
                host: "127.0.0.1".into(),
                port: 45831,
            };
            daemon_control(&start, result, "daemon 已停止或沒有可停止的 daemon")?;
        }
    };
    Ok(())
}
