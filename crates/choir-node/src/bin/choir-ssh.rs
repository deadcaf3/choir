//! `choir-ssh`: the forced command behind choir's git-over-SSH account
//! (D31). See [`choir_node::ssh`] for the design and the
//! `authorized_keys` line that installs it.
//!
//! ```text
//! choir-ssh --root <repo-root> --user <choir-user> [--acl-file <path>]
//!           [--handoff <path>] [--git-binary <path>]
//! ```
//!
//! Every flag comes from the forced command the operator wrote, never
//! from the client: sshd runs that command and puts whatever the client
//! asked for in `SSH_ORIGINAL_COMMAND`, which is the only input this
//! program takes from the far end. `--handoff` names the file the daemon
//! wrote at startup (`choir-node --ssh-handoff`); without it this account
//! serves fetches and refuses pushes, because an unsequenced push is
//! worse than no push.
//!
//! `--git-binary` exists because sshd runs the forced command through a
//! non-interactive login shell, whose `PATH` frequently lacks the git the
//! operator means: `/opt/homebrew/bin/git` is not on the default macOS
//! non-interactive path. Naming the binary is one line in
//! `authorized_keys` and removes the whole class of "works in my shell".

use std::path::PathBuf;

use choir_node::ssh::Shim;

/// Exit code for every refusal. git surfaces the message on stderr to
/// whoever ran the command, so the reason reaches a person.
const REFUSED: i32 = 1;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    for flag in [
        "--root",
        "--user",
        "--acl-file",
        "--handoff",
        "--git-binary",
    ] {
        if args.iter().any(|a| a == flag) && value(flag).is_none() {
            refuse(&format!("{flag} needs a value"));
        }
    }
    let (Some(root), Some(user)) = (value("--root"), value("--user")) else {
        refuse(
            "choir-ssh needs --root and --user; it is meant to be run by sshd as a forced command",
        );
    };
    let shim = Shim {
        root: PathBuf::from(root),
        user: user.clone(),
        acl_file: value("--acl-file").map(PathBuf::from),
        handoff: value("--handoff").map(PathBuf::from),
    };
    let git = value("--git-binary").unwrap_or_else(|| "git".to_string());

    // Set by sshd, and the one input that comes from the far end. Absent
    // means the client asked for a shell rather than for git.
    let Ok(original) = std::env::var("SSH_ORIGINAL_COMMAND") else {
        refuse(&format!(
            "hi {user}, your key works. This account serves git and has no shell; \
             clone with git@<host>:owner/repo.git"
        ));
    };
    let exec = match shim.decide(&original) {
        Ok(exec) => exec,
        Err(message) => refuse(&message),
    };

    let mut command = std::process::Command::new(&git);
    command.arg(exec.verb).arg(&exec.dir);
    for (key, value) in &exec.env {
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec`, not spawn: git then owns this process's stdin, stdout
        // and exit status directly, so the pack protocol and any signal
        // pass through with nothing in the middle to get them wrong. It
        // only returns on failure.
        let error = command.exec();
        refuse(&format!("could not run `{git}`: {error}"));
    }
    #[cfg(not(unix))]
    refuse("choir-ssh needs a unix host: it is an sshd forced command");
}

/// Prints one line to stderr and exits. Never returns.
fn refuse(message: &str) -> ! {
    eprintln!("choir: {message}");
    std::process::exit(REFUSED);
}
