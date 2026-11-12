use std::process::ExitCode;

use clap::Parser;
use fff_mcp_http::{Args, Config, default_log_file, spawn};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Runs at load time, before mimalloc maps its first arena.
#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    unsafe(link_section = ".init_array")
)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
static TUNE_MIMALLOC: extern "C" fn() = fff::tune_mimalloc;

fn main() -> ExitCode {
    let args = Args::parse();
    init_logging(&args);
    let config: Config = args.into();

    match spawn(config) {
        Ok(server) => {
            println!("fff-mcp-http listening on http://{}", server.addr());
            server.wait();
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: cannot bind: {error}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(args: &Args) {
    let hint = args.log_file.clone().unwrap_or_else(default_log_file);
    let Some(path) = hint.to_str() else {
        eprintln!("warning: log path is not valid UTF-8, file logging disabled");
        return;
    };
    if let Err(error) = fff::log::init_tracing(path, args.log_level.as_deref(), None) {
        eprintln!("warning: cannot start logging: {error}");
    }
}
