//! `supershuckie-frame-server`: pictures and sound of a replay for another program.
//!
//! ```text
//! supershuckie-frame-server                      serve on stdin/stdout (see docs/frame_server.md)
//! supershuckie-frame-server --probe <replay> [--layout N]
//!                                                describe a recording as JSON, no ROM needed
//! supershuckie-frame-server --version
//! ```
//!
//! The wire format is Cutter's `docs/frame-server-protocol.md`, version 1.

mod probe;
mod protocol;
mod server;
mod source;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: supershuckie-frame-server                       serve frames over stdin/stdout
       supershuckie-frame-server --probe <replay> [--layout N]
       supershuckie-frame-server --version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return server::serve();
    }

    match args[0].as_str() {
        "--version" | "-V" => {
            println!("{}, protocol {}", server::server_name(), protocol::PROTOCOL_VERSION);
            ExitCode::SUCCESS
        }
        "--help" | "-h" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "--probe" => {
            let mut replay: Option<PathBuf> = None;
            let mut layout: u8 = 0;
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--layout" => match rest.next().and_then(|v| v.parse::<u8>().ok()) {
                        Some(v) => layout = v,
                        None => return usage_error("--layout needs a number"),
                    },
                    _ if replay.is_none() => replay = Some(PathBuf::from(arg)),
                    _ => return usage_error(&format!("unexpected argument {arg}")),
                }
            }
            let Some(replay) = replay else {
                return usage_error("--probe needs a replay path");
            };
            match probe::probe(&replay, layout) {
                Ok(value) => {
                    println!("{value}");
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    println!("{}", serde_json::json!({ "error": message }));
                    ExitCode::from(1)
                }
            }
        }
        other => usage_error(&format!("unknown argument {other}")),
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("supershuckie-frame-server: {message}\n{USAGE}");
    ExitCode::from(2)
}
