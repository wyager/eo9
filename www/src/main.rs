//! eo9-www: serve the eo9.org site directly to the internet (plain HTTP for development,
//! HTTPS with ACME or provided certificates for production). All logic lives in the library
//! (`eo9_www`); this binary only reads the process environment, sets up the runtime, and
//! reports startup errors.

use std::env;
use std::process::ExitCode;

use eo9_www::{USAGE, parse_config, server};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let config = match parse_config(args, env_var) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("eo9-www: {message}");
            return ExitCode::from(2);
        }
    };

    // Make ring the process-wide rustls provider so every part of the TLS stack (ours and
    // rustls-acme's outbound ACME client) agrees on one provider. Ignore the error case:
    // it only means a default was already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("eo9-www: failed to start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(server::run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eo9-www: {message}");
            ExitCode::FAILURE
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}
