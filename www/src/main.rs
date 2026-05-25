//! eo9-www: serve the eo9.org static site. All logic lives in the library (`eo9_www`);
//! this binary only reads the process environment and reports startup errors.

use std::env;
use std::process::ExitCode;

use eo9_www::{Config, SiteServer, USAGE, parse_config};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let config = match parse_config(args, env_var("EO9_WWW_BIND"), env_var("EO9_WWW_SITE")) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("eo9-www: {message}");
            return ExitCode::from(2);
        }
    };

    match SiteServer::bind(&config) {
        Ok(server) => {
            announce(&config, &server);
            server.run();
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("eo9-www: {message}");
            ExitCode::FAILURE
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn announce(config: &Config, server: &SiteServer) {
    match server.local_addr() {
        Some(addr) => println!(
            "eo9-www: serving {} on http://{addr}/",
            config.site_root.display()
        ),
        None => println!(
            "eo9-www: serving {} on {}",
            config.site_root.display(),
            config.bind
        ),
    }
}
