//! `choir node install` — hand the node to whatever supervises services
//! on this machine, so it survives a logout, a crash and a reboot.
//!
//! # The unit names a command, not a configuration
//!
//! The rendered unit runs `choir node serve`, and nothing else. Every
//! path and flag the daemon needs is derived at start time from the
//! state directory, which means the supervision file never has to be
//! re-rendered because a flag changed — the failure mode of the shell
//! scripts this replaces, where a plist that lints clean and restarts
//! cleanly can still launch yesterday's arguments.
//!
//! # Examples
//!
//! ```
//! use choir_cli::supervise::Supervisor;
//!
//! // Whichever this machine has; the rendering is the same shape either way.
//! let unit = Supervisor::Launchd.render(
//!     std::path::Path::new("/usr/local/bin/choir"),
//!     std::path::Path::new("/home/example/.choir"),
//!     8417,
//!     &[],
//! );
//! assert!(unit.contains("choir"));
//! ```

use std::path::{Path, PathBuf};

/// The service manager this machine runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Supervisor {
    /// macOS, per-user LaunchAgent.
    Launchd,
    /// Linux, `systemd --user`.
    Systemd,
}

/// The reverse-DNS label and unit stem. One name, used by both, so the
/// thing to grep for is the same wherever it is running.
pub const LABEL: &str = "com.choir.node";

impl Supervisor {
    /// What this machine has, if it has one.
    ///
    /// By operating system rather than by probing for the binary: a mac
    /// without `launchctl` is a broken mac, and guessing `systemd` there
    /// would produce a unit file nothing will ever read.
    #[must_use]
    pub fn detect() -> Option<Supervisor> {
        match std::env::consts::OS {
            "macos" => Some(Supervisor::Launchd),
            "linux" => Some(Supervisor::Systemd),
            _ => None,
        }
    }

    /// Where the unit file belongs under `home`.
    #[must_use]
    pub fn unit_path(self, home: &Path) -> PathBuf {
        match self {
            Supervisor::Launchd => home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            Supervisor::Systemd => home.join(".config/systemd/user/choir-node.service"),
        }
    }

    /// The unit file's contents.
    ///
    /// `extra` is forwarded to the daemon after `--`, the same spelling
    /// a person would type, so a supervised node and a hand-started one
    /// are the same command with the same arguments.
    #[must_use]
    pub fn render(self, exe: &Path, state: &Path, port: u16, extra: &[String]) -> String {
        let log = state.join("node.log");
        match self {
            Supervisor::Launchd => {
                let mut argv = String::new();
                for arg in self.argv(exe, state, port, extra) {
                    argv.push_str(&format!("    <string>{}</string>\n", xml_escape(&arg)));
                }
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                     \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n\
                     <dict>\n  \
                       <key>Label</key><string>{LABEL}</string>\n  \
                       <key>ProgramArguments</key>\n  <array>\n{argv}  </array>\n  \
                       <key>RunAtLoad</key><true/>\n  \
                       <key>KeepAlive</key><true/>\n  \
                       <key>StandardOutPath</key><string>{log}</string>\n  \
                       <key>StandardErrorPath</key><string>{log}</string>\n\
                     </dict>\n\
                     </plist>\n",
                    log = xml_escape(&log.display().to_string()),
                )
            }
            Supervisor::Systemd => {
                let argv: Vec<String> = self
                    .argv(exe, state, port, extra)
                    .into_iter()
                    .map(|a| shell_quote(&a))
                    .collect();
                format!(
                    "[Unit]\n\
                     Description=choir node\n\
                     After=network.target\n\
                     \n\
                     [Service]\n\
                     ExecStart={exec}\n\
                     Restart=always\n\
                     RestartSec=2\n\
                     StandardOutput=append:{log}\n\
                     StandardError=append:{log}\n\
                     \n\
                     [Install]\n\
                     WantedBy=default.target\n",
                    exec = argv.join(" "),
                    log = log.display(),
                )
            }
        }
    }

    /// The command the unit runs, as argv.
    ///
    /// Shared by both renderings so the two supervisors cannot come to
    /// disagree about what a supervised node is.
    #[must_use]
    pub fn argv(self, exe: &Path, state: &Path, port: u16, extra: &[String]) -> Vec<String> {
        let mut argv = vec![
            exe.display().to_string(),
            "node".to_string(),
            "serve".to_string(),
            "--state".to_string(),
            state.display().to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        if !extra.is_empty() {
            argv.push("--".to_string());
            argv.extend_from_slice(extra);
        }
        argv
    }

    /// The commands that load, stop and remove the unit, in order.
    ///
    /// Returned as data rather than run here so a test can assert on
    /// what would be run without a service manager being involved, and
    /// so the caller can print them when it refuses.
    #[must_use]
    pub fn commands(self, action: Action, home: &Path) -> Vec<Vec<String>> {
        let unit = self.unit_path(home).display().to_string();
        let uid = users_id();
        match (self, action) {
            // `bootout` then `bootstrap`, never `kickstart -k`: a restart
            // reloads launchd's *cached* job definition, so a unit whose
            // arguments changed can restart cleanly and still run the old
            // ones.
            (Supervisor::Launchd, Action::Install) => vec![
                vec![
                    "launchctl".into(),
                    "bootout".into(),
                    format!("gui/{uid}/{LABEL}"),
                ],
                vec![
                    "launchctl".into(),
                    "bootstrap".into(),
                    format!("gui/{uid}"),
                    unit,
                ],
            ],
            (Supervisor::Launchd, Action::Stop) => vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LABEL}"),
            ]],
            (Supervisor::Launchd, Action::Uninstall) => vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LABEL}"),
            ]],
            (Supervisor::Systemd, Action::Install) => vec![
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    "choir-node.service".into(),
                ],
            ],
            (Supervisor::Systemd, Action::Stop) => vec![vec![
                "systemctl".into(),
                "--user".into(),
                "stop".into(),
                "choir-node.service".into(),
            ]],
            (Supervisor::Systemd, Action::Uninstall) => vec![vec![
                "systemctl".into(),
                "--user".into(),
                "disable".into(),
                "--now".into(),
                "choir-node.service".into(),
            ]],
        }
    }
}

/// What `commands` should produce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Load the unit and start it now.
    Install,
    /// Stop it for this boot, leaving the unit in place.
    Stop,
    /// Stop it and stop it coming back.
    Uninstall,
}

/// Whether this executable is sitting in a cargo build directory.
///
/// A unit pointing into one breaks at the next `cargo clean`, and it
/// breaks at reboot — the moment nobody is watching. Detected by the
/// profile directory cargo actually writes into rather than by looking
/// for a component called `target`: `CARGO_TARGET_DIR` renames that one,
/// and the first version of this check was defeated by a target
/// directory called anything else.
#[must_use]
pub fn in_build_directory(exe: &Path) -> bool {
    let Some(parent) = exe.parent().and_then(Path::file_name) else {
        return false;
    };
    parent == "debug" || parent == "release"
}

/// This process's user id, for the `gui/<uid>` domain launchctl wants.
fn users_id() -> String {
    // `id -u` rather than a libc call: this workspace adds dependencies
    // reluctantly, and the answer is a number a subprocess already
    // prints. It is asked once per command, not per request.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The five characters XML cannot carry raw.
///
/// A path with `&` in it is legal on every filesystem this runs on, and
/// a plist that contains one raw is a plist `launchctl` refuses to
/// parse — with an error naming the file rather than the character.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Quotes one `ExecStart` word.
///
/// systemd splits `ExecStart` on whitespace, so a state directory with a
/// space in it becomes two arguments and the node starts against a root
/// that does not exist.
fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c))
    {
        return word.to_string();
    }
    format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
}

// ------------------------------------------------------------- the timer

/// The label of the backup timer, and its unit stem.
pub const BACKUP_LABEL: &str = "com.choir.backup";

impl Supervisor {
    /// The command the timer runs, as argv: `choir backup take`.
    #[must_use]
    pub fn backup_argv(self, exe: &Path, state: &Path, dest: &Path) -> Vec<String> {
        vec![
            exe.display().to_string(),
            "backup".to_string(),
            "take".to_string(),
            dest.display().to_string(),
            "--state".to_string(),
            state.display().to_string(),
        ]
    }

    /// The files a scheduled backup is, under `home`: one plist, or a
    /// service and the timer that fires it.
    #[must_use]
    pub fn backup_units(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Supervisor::Launchd => {
                vec![home.join(format!("Library/LaunchAgents/{BACKUP_LABEL}.plist"))]
            }
            Supervisor::Systemd => vec![
                home.join(".config/systemd/user/choir-backup.service"),
                home.join(".config/systemd/user/choir-backup.timer"),
            ],
        }
    }

    /// The contents of each of [`Self::backup_units`], in the same order.
    ///
    /// The log is `backup.log` beside the node's own, so the record of
    /// every run is where `choir node logs` already looks.
    #[must_use]
    pub fn render_backup(
        self,
        exe: &Path,
        state: &Path,
        dest: &Path,
        every_secs: u64,
    ) -> Vec<String> {
        let log = state.join("backup.log");
        let argv = self.backup_argv(exe, state, dest);
        match self {
            Supervisor::Launchd => {
                let mut array = String::new();
                for arg in &argv {
                    array.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
                }
                vec![format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                     \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n\
                     <dict>\n  \
                       <key>Label</key><string>{BACKUP_LABEL}</string>\n  \
                       <key>ProgramArguments</key>\n  <array>\n{array}  </array>\n  \
                       <key>StartInterval</key><integer>{every_secs}</integer>\n  \
                       <key>StandardOutPath</key><string>{log}</string>\n  \
                       <key>StandardErrorPath</key><string>{log}</string>\n\
                     </dict>\n\
                     </plist>\n",
                    log = xml_escape(&log.display().to_string()),
                )]
            }
            Supervisor::Systemd => {
                let exec: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
                vec![
                    format!(
                        "[Unit]\n\
                         Description=choir backup\n\
                         \n\
                         [Service]\n\
                         Type=oneshot\n\
                         ExecStart={exec}\n\
                         StandardOutput=append:{log}\n\
                         StandardError=append:{log}\n",
                        exec = exec.join(" "),
                        log = log.display(),
                    ),
                    format!(
                        "[Unit]\n\
                         Description=choir backup every {every_secs}s\n\
                         \n\
                         [Timer]\n\
                         OnBootSec=60\n\
                         OnUnitActiveSec={every_secs}\n\
                         Unit=choir-backup.service\n\
                         \n\
                         [Install]\n\
                         WantedBy=timers.target\n"
                    ),
                ]
            }
        }
    }

    /// The commands that load or remove the timer, in order.
    ///
    /// `Stop` is `Uninstall` here: a timer has no "for this boot".
    #[must_use]
    pub fn backup_commands(self, action: Action, home: &Path) -> Vec<Vec<String>> {
        let uid = users_id();
        match (self, action) {
            (Supervisor::Launchd, Action::Install) => vec![
                vec![
                    "launchctl".into(),
                    "bootout".into(),
                    format!("gui/{uid}/{BACKUP_LABEL}"),
                ],
                vec![
                    "launchctl".into(),
                    "bootstrap".into(),
                    format!("gui/{uid}"),
                    self.backup_units(home)[0].display().to_string(),
                ],
            ],
            (Supervisor::Launchd, Action::Stop | Action::Uninstall) => vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{BACKUP_LABEL}"),
            ]],
            (Supervisor::Systemd, Action::Install) => vec![
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    "choir-backup.timer".into(),
                ],
            ],
            (Supervisor::Systemd, Action::Stop | Action::Uninstall) => vec![vec![
                "systemctl".into(),
                "--user".into(),
                "disable".into(),
                "--now".into(),
                "choir-backup.timer".into(),
            ]],
        }
    }
}
