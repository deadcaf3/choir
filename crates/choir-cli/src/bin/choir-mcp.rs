//! `choir-mcp` — synchronous stdio MCP adapter over a choir node.
//!
//! ```text
//! choir-mcp <api> [--auth-file <path>] [--auth-user <name>]
//! ```
//!
//! All configuration is explicit CLI input. The optional auth file uses
//! the node's `user:token`-per-line format; no environment variable is
//! read for configuration.

use choir_cli::mcp::{serve, HttpClient};

const USAGE: &str = "usage: choir-mcp <api> [--auth-file <path>] [--auth-user <name>]

  Speaks MCP over stdin and stdout. Point an agent's MCP configuration at
  it; it is not meant to be run at a prompt.
";

fn usage() -> ! {
    eprint!("{USAGE}");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Before the URL check, or `--help` is read as an address and
    // answered with a complaint about its scheme.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        std::process::exit(0);
    }
    let Some(api) = args.first() else {
        usage();
    };
    let mut auth_file = None;
    let mut auth_user = None;
    let mut index = 1;
    while index < args.len() {
        let Some(value) = args.get(index + 1) else {
            usage();
        };
        match args[index].as_str() {
            "--auth-file" if auth_file.is_none() => auth_file = Some(value.as_str()),
            "--auth-user" if auth_user.is_none() => auth_user = Some(value.as_str()),
            _ => usage(),
        }
        index += 2;
    }
    let client = match HttpClient::new(api, auth_file.map(std::path::Path::new), auth_user) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("choir-mcp: {error}");
            std::process::exit(2);
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(error) = serve(stdin.lock(), stdout, &client) {
        eprintln!("choir-mcp: stdio failed: {error}");
        std::process::exit(1);
    }
}
