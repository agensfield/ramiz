mod add;
mod cancellation;
mod cli;
mod doctor;
mod error;
mod fsclone;
mod git;
mod inherit;
mod output;
mod update;

use std::process::ExitCode;

use clap::Parser;

use crate::{
    cli::{Cli, Command, UpdateArgs},
    error::RamizError,
};

pub fn main_entry() -> ExitCode {
    if let Err(error) = cancellation::install() {
        eprintln!("ramiz: unable to install signal handlers: {error}");
        return ExitCode::FAILURE;
    }
    let arguments: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match Cli::try_parse_from(&arguments) {
        Ok(cli) => {
            let command = cli.command.name();
            match run(cli) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    error.emit(command);
                    ExitCode::from(error.exit_code())
                }
            }
        }
        Err(error) => {
            let code = error.exit_code();
            if code == 0 || !arguments.iter().any(|argument| argument == "--json") {
                let _ = error.print();
            } else {
                let command = arguments
                    .get(1)
                    .and_then(|argument| argument.to_str())
                    .and_then(|argument| match argument {
                        "add" => Some("add"),
                        "doctor" => Some("doctor"),
                        "update" => Some("update"),
                        _ => None,
                    })
                    .unwrap_or("unknown");
                RamizError::new("cli_usage", error.to_string(), true).emit(command);
            }
            ExitCode::from(u8::try_from(code).unwrap_or(2))
        }
    }
}

fn run(cli: Cli) -> Result<(), RamizError> {
    match cli.command {
        Command::Add(args) => add::run(args),
        Command::Doctor(args) => doctor::run(args),
        Command::Update(args) => run_update(args),
    }
}

fn run_update(args: UpdateArgs) -> Result<(), RamizError> {
    let executable = std::env::current_exe()
        .map_err(|error| RamizError::new("current_executable", error.to_string(), args.json))?;
    let request = update::UpdateRequest::new(args.check, executable, env!("CARGO_PKG_VERSION"))
        .with_adopt(args.adopt);
    let mut service = update::system_service();
    match service.run(&request) {
        Ok(result) => {
            if args.json {
                output::success("update", result, &[]);
            } else {
                println!("{}", result.message);
                for command in result.commands {
                    println!("run: {} {}", command.program, command.args.join(" "));
                }
            }
            Ok(())
        }
        Err(error) => {
            let details = serde_json::to_value(&error).expect("update failures are serializable");
            Err(RamizError::new(error.code, error.message, args.json).with_update_details(details))
        }
    }
}
