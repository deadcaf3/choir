//! Bridge v0 (DECISIONS.md D21, read-replica stage): mirror an upstream git
//! repo and record every upstream ref movement as a signed, CAS-checked
//! op through the choir platform API.
//!
//! Single-canonical invariant: the upstream forge is canonical; choir is
//! a follower of the sequencer. This binary never writes back to the
//! upstream — the write-back stage arrives with the GitHub App
//! credentials story (risk register #15).
//!
//! Usage:
//!   choir-bridge <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]
//!
//! - `mirror-path`: where the `git clone --mirror` lives (created on
//!   first run, `remote update --prune`d afterwards).
//! - `api-base`: e.g. `http://127.0.0.1:8417` — a choir-node with the
//!   platform API enabled and the bridge's public key registered.
//! - `bridge-key-file`: 32 secret bytes for the bridge's actor key
//!   (created 0600 on first run if absent).
//! - `label`: ref namespace prefix, e.g. `github/git/git`.
//! - `--once`: single sync instead of a 60 s loop.

use choir_bridge::github;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use choir_hash::ContentHash;
use choir_identity::ActorKey;
use choir_view::{OpKind, ViewOp};

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs git in the mirror, returning stdout or the failure text (a
/// transient fetch failure must not kill the sync loop).
fn git(args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Loopback API call via curl; returns (status, body).
fn api(method: &str, url: &str, body: Option<&str>) -> (u16, String) {
    let mut args = vec!["-sk", "-w", "\n%{http_code}", "-X", method];
    if let Some(b) = body {
        args.extend(["-d", b]);
    }
    args.push(url);
    let out = std::process::Command::new("curl")
        .args(&args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    match text.rsplit_once('\n') {
        Some((b, code)) => (code.trim().parse().unwrap_or(0), b.to_string()),
        None => (0, text),
    }
}

/// Current upstream refs of the mirror: refname -> oid hex.
fn mirror_refs(mirror: &Path) -> Result<BTreeMap<String, String>, String> {
    Ok(git(
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        Some(mirror),
    )?
    .lines()
    .filter_map(|l| {
        let (oid, name) = l.split_once(' ')?;
        Some((name.to_string(), oid.to_string()))
    })
    .collect())
}

/// Choir-side view of this label's refs: refname -> ContentHash.
fn view_refs(api_base: &str, label: &str) -> BTreeMap<String, ContentHash> {
    let (status, body) = api("GET", &format!("{api_base}/api/view"), None);
    assert_eq!(status, 200, "view: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("view json");
    let prefix = format!("{label}:");
    v["refs"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, hex)| {
                    let name = k.strip_prefix(&prefix)?;
                    let hex = hex.as_str()?;
                    let (codec, digest) = hex.split_once('-')?;
                    Some((
                        name.to_string(),
                        ContentHash {
                            codec: u8::from_str_radix(codec, 16).ok()?,
                            digest: (0..digest.len())
                                .step_by(2)
                                .map(|i| u8::from_str_radix(&digest[i..i + 2], 16).ok())
                                .collect::<Option<Vec<u8>>>()?,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A `<codec>-<digest>` content hash as the node serves it.
fn parse_hash(hex: &str) -> Option<ContentHash> {
    let (codec, digest) = hex.split_once('-')?;
    Some(ContentHash {
        codec: u8::from_str_radix(codec, 16).ok()?,
        digest: (0..digest.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(digest.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()?,
    })
}

/// The log identity every op of this round is signed for: `(node, head)`.
///
/// A whole batch may share one head — a scope is admissible while the
/// head it names is still in the node's window, not only while it is the
/// tip — so this is one read per sync round, not one per op.
///
/// # Panics
///
/// Panics when the node serves no `log.node`, which means it predates op
/// scopes. Mirroring into a log that cannot bind a signature to itself is
/// the situation this exists to prevent, so it fails loudly rather than
/// signing unscoped ops.
fn view_scope(api_base: &str) -> (ContentHash, Option<ContentHash>) {
    let (status, body) = api("GET", &format!("{api_base}/api/view"), None);
    assert_eq!(status, 200, "view: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("view json");
    let node = v["log"]["node"]
        .as_str()
        .and_then(parse_hash)
        .expect("node serves log.node; one that does not predates op scopes");
    let head = v["log"]["head"].as_str().and_then(parse_hash);
    (node, head)
}

/// Ops per `/api/submit-batch` request; keeps request bodies well under
/// a megabyte.
const BATCH: usize = 500;

/// Signs one op into its submit-request JSON object.
fn signed_op(key: &ActorKey, channel: &str, op: ViewOp) -> serde_json::Value {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "channel": channel,
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
}

/// Submits ops in batches; returns the accepted count, printing each
/// rejection. Request bodies go through a temp file — 500 signed ops
/// exceed argv limits.
fn submit_batch(api_base: &str, ops: &[serde_json::Value]) -> Result<usize, String> {
    let mut accepted = 0;
    for chunk in ops.chunks(BATCH) {
        let body = serde_json::json!({ "ops": chunk }).to_string();
        let tmp = std::env::temp_dir().join(format!(
            "choir-bridge-batch-{}-{}",
            std::process::id(),
            accepted
        ));
        std::fs::write(&tmp, &body).map_err(|e| format!("write batch: {e}"))?;
        let out = std::process::Command::new("curl")
            .args(["-sk", "-X", "POST", "--data-binary"])
            .arg(format!("@{}", tmp.display()))
            .arg(format!("{api_base}/api/submit-batch"))
            .output()
            .map_err(|e| format!("spawn curl: {e}"))?;
        std::fs::remove_file(&tmp).ok();
        let resp: serde_json::Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("batch response: {e}"))?;
        accepted += resp["accepted"].as_u64().unwrap_or(0) as usize;
        if resp["rejected"].as_u64().unwrap_or(0) > 0 {
            for r in resp["results"].as_array().into_iter().flatten() {
                if let Some(err) = r.get("error").and_then(|e| e.as_str()) {
                    eprintln!("bridge: op rejected: {err}");
                }
            }
        }
    }
    Ok(accepted)
}

/// One sync round: fetch upstream, diff against the choir view, submit
/// the delta in batches. Returns (set, deleted, unchanged).
fn sync_once(
    upstream: &str,
    mirror: &Path,
    api_base: &str,
    key: &ActorKey,
    label: &str,
) -> Result<(usize, usize, usize), String> {
    if mirror.join("HEAD").exists() {
        git(&["remote", "update", "--prune"], Some(mirror))?;
    } else {
        std::fs::create_dir_all(mirror.parent().unwrap_or(Path::new(".")))
            .map_err(|e| format!("create mirror dir: {e}"))?;
        git(
            &[
                "clone",
                "--mirror",
                upstream,
                mirror.to_str().expect("utf8 path"),
            ],
            None,
        )?;
    }

    let upstream_refs = mirror_refs(mirror)?;
    let choir_refs = view_refs(api_base, label);
    let (node, head) = view_scope(api_base);
    let workspace = format!("bridge/{label}");

    let mut sets = Vec::new();
    let mut unchanged = 0;
    for (name, oid) in &upstream_refs {
        let commit = match ContentHash::from_git_oid(oid) {
            Some(c) => c,
            None => continue, // non-oid ref (should not happen)
        };
        let prev = choir_refs.get(name);
        if prev == Some(&commit) {
            unchanged += 1;
            continue;
        }
        let op = ViewOp::new(OpKind::SetRef {
            name: format!("{label}:{name}"),
            commit,
            prev: prev.cloned(),
        })
        .in_scope(node.clone(), head.clone());
        sets.push(signed_op(key, &workspace, op));
    }
    let mut deletes = Vec::new();
    for (name, prev) in &choir_refs {
        if !upstream_refs.contains_key(name) {
            let op = ViewOp::new(OpKind::DeleteRef {
                name: format!("{label}:{name}"),
                prev: Some(prev.clone()),
            })
            .in_scope(node.clone(), head.clone());
            deletes.push(signed_op(key, &workspace, op));
        }
    }
    let set = submit_batch(api_base, &sets)?;
    let deleted = submit_batch(api_base, &deletes)?;
    Ok((set, deleted, unchanged))
}

/// How long to wait for CI on the train commit before giving up.
const CI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Poll interval while waiting on CI.
const CI_POLL: std::time::Duration = std::time::Duration::from_secs(15);
/// How long to watch the landed tip for post-land CI (D23: train green
/// is necessary, never sufficient — the default-branch push can trigger
/// branch-conditional workflows the train branch never ran).
const POST_LAND_WATCH: std::time::Duration = std::time::Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DifferentialConfig {
    runner: PathBuf,
    command: PathBuf,
    state: PathBuf,
}

/// Where a round's check verdict comes from when it is not the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CiConfig {
    /// Versioned command file: the argv, declared environment, and
    /// timeout the train is checked with. The same format the
    /// differential detector reads, because it is the same question —
    /// "what does an operator want run against a tree".
    command: PathBuf,
    /// Helper to speak the D18 protocol to, or `None` to run the
    /// command as a child of this process.
    runner: Option<PathBuf>,
    /// Check each change on its own speculative state through the D5
    /// merge queue, instead of checking one train commit once.
    ///
    /// It sits here rather than beside it because it is only
    /// meaningful with a command: the forge answers about a ref, so
    /// asking it once per train member would be the same answer
    /// repeated under different names. Being a field of this struct is
    /// what makes that structural instead of a rule to remember.
    speculate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueArgs {
    positional: Vec<String>,
    land: bool,
    watch: Option<u64>,
    differential: Option<DifferentialConfig>,
    ci: Option<CiConfig>,
}

fn parse_queue_args(args: &[String]) -> Result<QueueArgs, String> {
    let mut land = false;
    let mut watch = None;
    let mut runner = None;
    let mut command = None;
    let mut state = None;
    let mut ci_command = None;
    let mut ci_runner = None;
    let mut speculate = false;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let next_path = |value: Option<&String>, flag: &str| {
            value
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} needs a path"))
        };
        match arg.as_str() {
            "--land" => land = true,
            "--watch" => {
                watch = Some(
                    it.next()
                        .ok_or("--watch needs an interval in seconds")?
                        .parse()
                        .map_err(|_| "--watch needs an interval in seconds")?,
                );
            }
            "--differential-runner" => {
                if runner.is_some() {
                    return Err("--differential-runner may be supplied only once".to_string());
                }
                runner = Some(next_path(it.next(), "--differential-runner")?);
            }
            "--differential-command" => {
                if command.is_some() {
                    return Err("--differential-command may be supplied only once".to_string());
                }
                command = Some(next_path(it.next(), "--differential-command")?);
            }
            "--speculate" => speculate = true,
            "--ci-command" => {
                if ci_command.is_some() {
                    return Err("--ci-command may be supplied only once".to_string());
                }
                ci_command = Some(next_path(it.next(), "--ci-command")?);
            }
            "--ci-runner" => {
                if ci_runner.is_some() {
                    return Err("--ci-runner may be supplied only once".to_string());
                }
                ci_runner = Some(next_path(it.next(), "--ci-runner")?);
            }
            "--differential-state" => {
                if state.is_some() {
                    return Err("--differential-state may be supplied only once".to_string());
                }
                state = Some(next_path(it.next(), "--differential-state")?);
            }
            value if value.starts_with("--") => {
                return Err(format!("unknown queue option {value}"));
            }
            value => positional.push(value.to_string()),
        }
    }
    let differential = match (runner, command, state) {
        (None, None, None) => None,
        (Some(runner), Some(command), Some(state)) => Some(DifferentialConfig {
            runner,
            command,
            state,
        }),
        _ => {
            return Err(
                "differential mode needs runner, command, and state paths together".to_string(),
            );
        }
    };
    // A runner with no command would hand a helper nothing to run and
    // report an outage, which reads as our fault rather than as the
    // invocation error it is.
    let ci = match (ci_command, ci_runner) {
        (None, None) => None,
        (Some(command), runner) => Some(CiConfig {
            command,
            runner,
            speculate,
        }),
        (None, Some(_)) => {
            return Err("--ci-runner needs --ci-command to have something to run".to_string());
        }
    };
    if speculate && ci.is_none() {
        return Err(
            "--speculate needs --ci-command: the forge cannot answer per change".to_string(),
        );
    }
    Ok(QueueArgs {
        positional,
        land,
        watch,
        differential,
        ci,
    })
}

/// One queue-as-bot round (D21 queue stage, verdict-only): fetch the
/// open PRs, build the speculative train locally, publish it as the
/// `choir/train` branch so the forge's CI runs on it, wait for the
/// check verdict, and post a per-PR `choir/queue` commit status. With
/// `land`, a green train is then fast-forwarded onto the default
/// branch (non-force push: a lost race is a rejection, not a clobber).
/// One speculative-train round.
///
/// **Decision inputs are structured only** (risk #16, Rule of Two): the
/// train is built from PR numbers and git oids, the verdict comes from CI
/// `status`/`conclusion` enums or from a [`choir_queue::executor::Verdict`],
/// and landing is gated on that verdict plus the caller's `land`
/// argument. No pull-request title, body, branch
/// name, author, commit message, or check-run text reaches any
/// conditional here. `land` is passed in from a per-invocation `--land`
/// flag and has no configuration default, so a bridge started without it
/// cannot be talked into landing anything. The executor's own strings
/// are held to the same rule: a `Verdict::Errored` detail is printed for
/// an operator and never interpolated into a commit status or read by a
/// branch, so what a helper says cannot become what the bridge does.
///
/// With `ci`, the check signal is ours (D18) instead of the forge's:
/// nothing speculative is pushed, no `choir/train` branch appears on the
/// remote, and the verdict comes from the executor the operator named.
/// With `ci.speculate` on top of that, the round goes through the D5
/// merge queue instead ([`speculative_round`]) and every change gets
/// its own verdict rather than sharing the train's.
fn queue_round(
    app_id: &str,
    pem: &Path,
    repo: &str,
    workdir: &Path,
    land: bool,
    differential: Option<&DifferentialConfig>,
    ci: Option<&CiConfig>,
) -> Result<(), String> {
    // Loaded before anything is fetched or pushed: a command file with a
    // typo in it should cost an error message, not a train on the remote.
    let ci_spec = match ci {
        Some(config) => Some(choir_queue::differential_ledger::load_command(
            &config.command,
        )?),
        None => None,
    };
    let token = github::app_jwt(app_id, pem).and_then(|jwt| github::installation_token(&jwt))?;
    let base_branch = github::default_branch(&token, repo)?;
    let prs = github::list_open_prs(&token, repo)?;
    if prs.is_empty() {
        println!("queue: no open PRs on {repo}; nothing to do");
        return Ok(());
    }
    // The installation token lives in this URL for the duration of the
    // round; it is passed per-invocation and never written to git
    // config or disk.
    let url = format!("https://x-access-token:{token}@github.com/{repo}.git");

    if !workdir.join(".git").exists() {
        std::fs::create_dir_all(workdir).map_err(|e| format!("create workdir: {e}"))?;
        git(&["init", "-q"], Some(workdir))?;
    }
    let mut fetch: Vec<String> = vec!["fetch".into(), "-q".into(), url.clone()];
    fetch.push(format!("+refs/heads/{base_branch}:refs/choirq/base"));
    for pr in &prs {
        fetch.push(format!(
            "+refs/pull/{}/head:refs/choirq/pr/{}",
            pr.number, pr.number
        ));
    }
    let fetch_refs: Vec<&str> = fetch.iter().map(String::as_str).collect();
    git(&fetch_refs, Some(workdir))?;
    let base = git(&["rev-parse", "refs/choirq/base"], Some(workdir))?
        .trim()
        .to_string();

    let heads: Vec<(u64, String)> = prs
        .iter()
        .map(|p| (p.number, format!("refs/choirq/pr/{}", p.number)))
        .collect();
    if let (Some(config), Some(spec)) = (ci.filter(|c| c.speculate), ci_spec.as_ref()) {
        return speculative_round(
            &token,
            repo,
            workdir,
            &url,
            &base,
            &base_branch,
            &heads,
            spec,
            config,
            land,
        );
    }
    let train = choir_bridge::queue::build_train(workdir, &base, &heads)?;
    if let Some(config) = differential {
        for (entry_id, result) in choir_bridge::queue::run_train_differentials(
            workdir,
            &train,
            &config.runner,
            &config.command,
            &config.state,
        ) {
            match result {
                Ok(outcome) => println!(
                    "queue: PR #{}: advisory differential {} (observation {}, pending {})",
                    entry_id,
                    outcome.verdict.as_str(),
                    outcome.observation_id,
                    outcome.pending_interactions,
                ),
                Err(error) => eprintln!(
                    "queue: PR #{}: advisory differential unavailable: {error}",
                    entry_id
                ),
            }
        }
    }
    if train.tip == base {
        println!("queue: no PR merged cleanly; train == base, skipping CI");
    } else if ci.is_some() {
        println!(
            "queue: train {} built locally ({} PRs considered)",
            train.tip,
            prs.len()
        );
    } else {
        // Only the forge path needs this: the branch exists so that
        // someone else's runners see the train. Checking it here means
        // the speculative merge of every open PR never leaves the
        // machine.
        git(
            &[
                "push",
                "-q",
                &url,
                &format!("+{}:refs/heads/choir/train", train.tip),
            ],
            Some(workdir),
        )?;
        println!(
            "queue: train {} pushed ({} PRs considered)",
            train.tip,
            prs.len()
        );
    }

    // The two signal sources converge here, into the three things the
    // rest of the round needs: whether to land, and what to tell each
    // PR. Landing consults `green` and nothing else, so neither source
    // gets its own landing rule.
    let report = if train.tip == base {
        // Nothing new to test; base is presumed already checked.
        choir_bridge::queue::train_report(&choir_queue::executor::Verdict::Passed)
    } else if let (Some(config), Some(spec)) = (ci, ci_spec.as_ref()) {
        let mut executor = build_executor(config)?;
        match choir_bridge::queue::run_train_ci(workdir, &train.tip, spec, executor.as_mut()) {
            Ok(verdict) => {
                println!("queue: train CI: {verdict}");
                choir_bridge::queue::train_report(&verdict)
            }
            Err(error) => {
                // Printed for an operator, never posted: the detail is a
                // string the executor chose.
                eprintln!("queue: train CI could not run: {error}");
                choir_bridge::queue::train_unavailable()
            }
        }
    } else {
        let deadline = std::time::Instant::now() + CI_TIMEOUT;
        let verdict = loop {
            let v = github::check_verdict(&token, repo, &train.tip)?;
            match v {
                github::Verdict::Success | github::Verdict::Failure => break v,
                github::Verdict::Pending | github::Verdict::NoRuns => {
                    if std::time::Instant::now() >= deadline {
                        break v;
                    }
                    std::thread::sleep(CI_POLL);
                }
            }
        };
        // The forge's four cases land on the same three states, by the
        // same rule: only a real red build blames the change.
        match verdict {
            github::Verdict::Success => choir_bridge::queue::TrainReport {
                green: true,
                state: "success",
                description: "speculative train green",
            },
            github::Verdict::Failure => choir_bridge::queue::TrainReport {
                green: false,
                state: "failure",
                description: "train CI failed",
            },
            github::Verdict::Pending => choir_bridge::queue::TrainReport {
                green: false,
                state: "error",
                description: "train CI timed out",
            },
            github::Verdict::NoRuns => choir_bridge::queue::TrainReport {
                green: false,
                state: "error",
                description: "no CI signal on train",
            },
        }
    };

    for entry in &train.entries {
        // Statuses land on the PR head sha, which the fetched ref points at.
        let sha = git(&["rev-parse", &entry.head], Some(workdir))?
            .trim()
            .to_string();
        let (state, desc) = if entry.already_landed {
            // Recognized by patch identity, not re-merged (item 4): the
            // change is in, so reporting a failure here would ask the
            // author to fix work that already landed.
            ("success", entry.note.as_str())
        } else if !entry.merged {
            ("failure", entry.note.as_str())
        } else {
            (report.state, report.description)
        };
        github::post_status(&token, repo, &sha, "choir/queue", state, desc)?;
        println!("queue: PR #{}: {state} ({desc})", entry.id);
    }
    if land && train.tip != base && report.green {
        choir_bridge::queue::land(workdir, &url, &train.tip, &base_branch)?;
        println!("queue: landed train {} -> {base_branch}", train.tip);
        if ci.is_some() {
            // Our executor tested exactly the commit that landed, and
            // the land is a fast-forward, so the base tip is the tree
            // that passed. There is no second, independent run for this
            // watch to observe. The cost is real and stated rather than
            // hidden: a nondeterministic failure the pre-land run missed
            // now lands, where the forge path's second look would
            // sometimes have caught it.
            println!("queue: local CI tested the landed commit; no post-land watch");
            return Ok(());
        }
        // Immediately after the push the aggregated verdict is still the
        // pre-land green, so an early Success is only trusted once a new
        // run has been seen Pending; otherwise watch the full window.
        let deadline = std::time::Instant::now() + POST_LAND_WATCH;
        let mut saw_pending = false;
        loop {
            let v = github::check_verdict(&token, repo, &train.tip)?;
            match v {
                github::Verdict::Failure => {
                    let new_tip = choir_bridge::queue::revert_train(
                        workdir,
                        &url,
                        &base,
                        &train.tip,
                        &base_branch,
                    )?;
                    println!("queue: post-land CI red; reverted train, {base_branch} -> {new_tip}");
                    for entry in train.entries.iter().filter(|e| e.merged) {
                        let sha = git(&["rev-parse", &entry.head], Some(workdir))?
                            .trim()
                            .to_string();
                        github::post_status(
                            &token,
                            repo,
                            &sha,
                            "choir/queue",
                            "failure",
                            "landed train reverted: post-land CI red",
                        )?;
                        println!("queue: PR #{}: reverted", entry.id);
                    }
                    break;
                }
                github::Verdict::Success if saw_pending => break,
                github::Verdict::Pending | github::Verdict::NoRuns | github::Verdict::Success => {
                    saw_pending |= v == github::Verdict::Pending;
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(CI_POLL);
                }
            }
        }
    }
    Ok(())
}

/// One round through the D5 merge queue: a verdict per change.
///
/// The train path asks CI once about one commit, so one red change
/// makes every open PR red and nothing lands. The queue tests each
/// change against the state produced by everything ahead of it, lands
/// the green prefix, blames the change that failed, and narrows the
/// window. That is the difference D5 is about, and it needs an
/// executor of ours: the forge answers about a ref, not about a change.
///
/// **Decision inputs are structured only** (Rule of Two), as in
/// [`queue_round`]: PR numbers, git oids, and a
/// [`choir_bridge::queue::PrOutcome`] decide everything. A stall
/// reason is a string an executor chose, so it is printed for an
/// operator and never posted or read by a branch.
#[allow(clippy::too_many_arguments)]
fn speculative_round(
    token: &str,
    repo: &str,
    workdir: &Path,
    url: &str,
    base: &str,
    base_branch: &str,
    heads: &[(u64, String)],
    spec: &choir_queue::differential_ledger::CommandSpec,
    config: &CiConfig,
    land: bool,
) -> Result<(), String> {
    // Checkouts go outside the repository on purpose: a worktree added
    // inside it is untracked files in every job's view of the tree.
    let checkouts =
        std::env::temp_dir().join(format!("choir-queue-checkouts-{}", std::process::id()));
    let mut executor: Box<dyn choir_queue::executor::CiExecutor> = match &config.runner {
        // The default provider must materialize each subject itself:
        // every train member is a different tree, so a runner working
        // in one directory would test the last one repeatedly.
        None => Box::new(choir_queue::worktree::WorktreeRunner::new(
            workdir.to_path_buf(),
            checkouts,
        )),
        Some(path) => {
            let program = path
                .to_str()
                .ok_or_else(|| format!("--ci-runner path {path:?} is not UTF-8"))?;
            Box::new(choir_queue::remote::ProtocolRunner::new(vec![
                program.to_string()
            ]))
        }
    };
    let round = choir_bridge::queue::run_queue(workdir, base, heads, spec, executor.as_mut())?;
    if let Some(why) = &round.stalled {
        // An operator's line. Nothing here reaches a status or a branch.
        eprintln!("queue: round stalled: {why}");
    }
    println!(
        "queue: speculative round over {} PRs; tip {}",
        heads.len(),
        round.tip
    );

    for (id, outcome) in &round.outcomes {
        let report = choir_bridge::queue::pr_report(outcome);
        let head = heads
            .iter()
            .find(|(n, _)| n == id)
            .map(|(_, r)| r.as_str())
            .ok_or_else(|| format!("no head for PR #{id}"))?;
        let sha = git(&["rev-parse", head], Some(workdir))?.trim().to_string();
        github::post_status(
            token,
            repo,
            &sha,
            "choir/queue",
            report.state,
            report.description,
        )?;
        println!("queue: PR #{id}: {} ({})", report.state, report.description);
    }

    if land && round.tip != base {
        choir_bridge::queue::land(workdir, url, &round.tip, base_branch)?;
        println!("queue: landed {} -> {base_branch}", round.tip);
        // No post-land watch, for the same reason the train path skips
        // one with a local executor: every landed change was tested on
        // the state it lands in, and the push is a fast-forward.
    }
    Ok(())
}

/// The executor a round checks its train with (D18).
///
/// Default is `LocalRunner`, which runs the command as a child of this
/// process: no isolation beyond what the OS gives a subprocess, and
/// therefore appropriate only where the PRs are trusted. `--ci-runner`
/// points at a helper instead, which is where a sandbox or a microVM
/// goes — the bridge cannot tell the two apart and does not need to.
///
/// # Errors
///
/// The runner path is not UTF-8, so it cannot be sent as the argv the
/// protocol carries.
fn build_executor(config: &CiConfig) -> Result<Box<dyn choir_queue::executor::CiExecutor>, String> {
    match &config.runner {
        None => Ok(Box::new(choir_queue::local::LocalRunner::new())),
        Some(path) => {
            let program = path
                .to_str()
                .ok_or_else(|| format!("--ci-runner path {path:?} is not UTF-8"))?;
            Ok(Box::new(choir_queue::remote::ProtocolRunner::new(vec![
                program.to_string(),
            ])))
        }
    }
}

/// Loads the 32-byte actor key at `path`, creating it (0600) if absent.
fn load_or_create_key(path: &str) -> ActorKey {
    if Path::new(path).exists() {
        let bytes = std::fs::read(path).expect("read key file");
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .expect("key file must be 32 bytes");
        ActorKey::from_secret_bytes(&bytes)
    } else {
        let key = ActorKey::generate();
        // Atomic and 0600 from creation: no window where the secret is
        // world-readable or half-written.
        choir_fs::write_atomic_private(Path::new(path), key.secret_bytes())
            .expect("write key file");
        key
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `choir-bridge --pubkey <key-file>`: ensure the key exists and print
    // its public key hex (for the daemon's --keys-file), then exit.
    if args.first().map(String::as_str) == Some("--pubkey") {
        let key_file = args.get(1).expect("--pubkey needs a key file path");
        let key = load_or_create_key(key_file);
        println!("{}", hex_encode(&key.public_key_bytes()));
        return;
    }
    // `choir-bridge app-debug <app-id> <pem-path>`: print the accepted
    // permission set of each installation.
    if args.first().map(String::as_str) == Some("app-debug") {
        let (Some(app_id), Some(pem)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: choir-bridge app-debug <app-id> <pem-path>");
            std::process::exit(2);
        };
        match github::app_jwt(app_id, Path::new(pem))
            .and_then(|jwt| github::installations_debug(&jwt))
        {
            Ok(body) => {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                for i in v.as_array().into_iter().flatten() {
                    println!("installation {}: permissions {}", i["id"], i["permissions"]);
                }
            }
            Err(e) => {
                eprintln!("app-debug failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // `choir-bridge post-status <app-id> <pem-path> <owner/repo> <sha> <state> <description>`
    // Write-back v0: one commit status through the GitHub App flow.
    if args.first().map(String::as_str) == Some("post-status") {
        let [app_id, pem, repo, sha, state, desc] = match &args[1..] {
            [a, b, c, d, e, f] => [a, b, c, d, e, f],
            _ => {
                eprintln!("usage: choir-bridge post-status <app-id> <pem-path> <owner/repo> <sha> <state> <description>");
                std::process::exit(2);
            }
        };
        let result = github::app_jwt(app_id, Path::new(pem))
            .and_then(|jwt| github::installation_token(&jwt))
            .and_then(|token| {
                let sha = if sha == "HEAD" {
                    github::head_sha(&token, repo)?
                } else {
                    sha.clone()
                };
                github::post_status(&token, repo, &sha, "choir/bridge", state, desc).map(|()| sha)
            });
        match result {
            Ok(sha) => println!("status posted: {repo}@{sha} -> {state}"),
            Err(e) => {
                eprintln!("post-status failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // `choir-bridge calibrate <repo> <runner> <command> <state> <rounds> <merge>...`
    // Replays already-existing real merge commits through the same advisory
    // three-worktree adapter used by queue mode. It never contacts a forge or
    // consumes a landing decision.
    if args.first().map(String::as_str) == Some("calibrate") {
        const CALIBRATE_USAGE: &str = "usage: choir-bridge calibrate [--fresh-worktrees] <repo> <runner> <command-file> <state-dir> <rounds> <merge>...";
        // One flag, recognised anywhere among the arguments: the positionals
        // are paths, a round count and hex merge ids, so none of them can
        // collide with it.
        let mut fresh_worktrees = false;
        let mut positional: Vec<&String> = Vec::new();
        for arg in &args[1..] {
            if arg == "--fresh-worktrees" {
                fresh_worktrees = true;
            } else {
                positional.push(arg);
            }
        }
        let [repo, runner, command_file, state_dir, rounds, merges @ ..] = positional.as_slice()
        else {
            eprintln!("{CALIBRATE_USAGE}");
            std::process::exit(2);
        };
        if merges.is_empty() {
            eprintln!("{CALIBRATE_USAGE}");
            std::process::exit(2);
        }
        let rounds = rounds.parse::<u64>().unwrap_or_else(|_| {
            eprintln!("calibration rounds must be a positive integer");
            std::process::exit(2);
        });
        if rounds == 0 {
            eprintln!("calibration rounds must be a positive integer");
            std::process::exit(2);
        }
        let repo = Path::new(repo.as_str());
        // One session for the whole run, so the build directories inside the
        // three worktrees survive from one observation to the next.
        // `--fresh-worktrees` opts back into a worktree pair-up per
        // observation, which is what an operator wants when re-checking a
        // flagged interaction against a pristine tree.
        let mut session =
            (!fresh_worktrees).then(|| choir_bridge::queue::DifferentialSession::open(repo));
        let mut failure = None;
        'rounds: for round in 1..=rounds {
            for merge in merges {
                let result = match session.as_mut() {
                    Some(session) => choir_bridge::queue::run_differential_in(
                        session,
                        merge.as_str(),
                        Path::new(runner.as_str()),
                        Path::new(command_file.as_str()),
                        Path::new(state_dir.as_str()),
                    ),
                    None => choir_bridge::queue::run_differential(
                        repo,
                        merge.as_str(),
                        Path::new(runner.as_str()),
                        Path::new(command_file.as_str()),
                        Path::new(state_dir.as_str()),
                    ),
                };
                match result {
                    Ok(outcome) => println!(
                        "calibration: round {round}: {merge}: {} (observation {}, pending {})",
                        outcome.verdict.as_str(),
                        outcome.observation_id,
                        outcome.pending_interactions
                    ),
                    Err(error) => {
                        failure = Some(format!("calibration failed for {merge}: {error}"));
                        break 'rounds;
                    }
                }
            }
        }
        // Close before exiting: `std::process::exit` skips `Drop`, so exiting
        // straight from the error arm would leave the worktrees behind.
        let cleanup = session.map_or(Ok(()), choir_bridge::queue::DifferentialSession::close);
        if let Some(error) = failure {
            eprintln!("{error}");
            std::process::exit(1);
        }
        if let Err(error) = cleanup {
            eprintln!("calibration cleanup failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    // `choir-bridge harvest [--fresh-worktrees] [--limit <n>] <repo> <runner> <command-file> <state-dir>`
    // D27: calibrate without the hand-picked merge list — enumerate every
    // two-parent merge on the first-parent mainline and replay each one
    // through the same advisory adapter. Unlike calibrate, a per-merge
    // failure is reported and the loop continues: a foreign history is
    // expected to hold revisions that no longer build, and one of them
    // must not cost the rest of the corpus. Merges already in the state
    // directory's ledger are skipped (incremental re-runs), and a first
    // interaction_failure verdict triggers reproduction runs that are
    // packaged as a specimen under <state-dir>/specimens/.
    if args.first().map(String::as_str) == Some("harvest") {
        const HARVEST_USAGE: &str =
            "usage: choir-bridge harvest [--fresh-worktrees] [--limit <n>] [--skip-inert] [--stop-after-inconclusive <n>] <repo> <runner> <command-file> <state-dir>";
        let mut fresh_worktrees = false;
        let mut limit = 0usize;
        let mut skip_inert = false;
        let mut stop_after_inconclusive = 0usize;
        let mut positional: Vec<&String> = Vec::new();
        let mut rest = args[1..].iter();
        while let Some(arg) = rest.next() {
            if arg == "--fresh-worktrees" {
                fresh_worktrees = true;
            } else if arg == "--skip-inert" {
                skip_inert = true;
            } else if arg == "--limit" || arg == "--stop-after-inconclusive" {
                let value = rest.next().unwrap_or_else(|| {
                    eprintln!("{HARVEST_USAGE}");
                    std::process::exit(2);
                });
                let parsed = value.parse().unwrap_or_else(|_| {
                    eprintln!("{arg} must be a non-negative integer; 0 disables it");
                    std::process::exit(2);
                });
                if arg == "--limit" {
                    limit = parsed;
                } else {
                    stop_after_inconclusive = parsed;
                }
            } else {
                positional.push(arg);
            }
        }
        let [repo, runner, command_file, state_dir] = positional.as_slice() else {
            eprintln!("{HARVEST_USAGE}");
            std::process::exit(2);
        };
        let repo = Path::new(repo.as_str());
        let merges = choir_bridge::queue::harvestable_merges(repo, limit).unwrap_or_else(|error| {
            eprintln!("harvest: {error}");
            std::process::exit(1);
        });
        if merges.is_empty() {
            println!("harvest: no two-parent merges in first-parent history");
            return;
        }
        let total = merges.len();
        let state = Path::new(state_dir.as_str());
        // Incremental: a merge whose oid is already a `revisions.merged`
        // in this state directory's ledger was harvested by an earlier
        // run; replay only the rest.
        let known = choir_bridge::queue::observed_merges(state);
        let mut session =
            (!fresh_worktrees).then(|| choir_bridge::queue::DifferentialSession::open(repo));
        let run_once = |session: &mut Option<choir_bridge::queue::DifferentialSession>,
                        merge: &str| match session.as_mut() {
            Some(session) => choir_bridge::queue::run_differential_in(
                session,
                merge,
                Path::new(runner.as_str()),
                Path::new(command_file.as_str()),
                state,
            ),
            None => choir_bridge::queue::run_differential(
                repo,
                merge,
                Path::new(runner.as_str()),
                Path::new(command_file.as_str()),
                state,
            ),
        };
        let mut observed = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut inert = Vec::new();
        let mut consecutive_inconclusive = 0usize;
        let mut stopped_early = false;
        // Oldest first: along a first-parent corpus the next merge's parent a
        // is often the previous observation's merge, so a held session's
        // parent-a checkout is a no-op (see DifferentialSession).
        for (index, merge) in merges.iter().rev().enumerate() {
            let number = index + 1;
            if known.contains(merge) {
                skipped += 1;
                continue;
            }
            // A merge whose whole union diff is documentation cannot fail a
            // build-and-test command its parents pass. Restricting the
            // population is recorded below, never silent.
            if skip_inert
                && choir_bridge::queue::merge_changes_only_inert_paths(repo, merge.as_str())
                    .unwrap_or(false)
            {
                inert.push(merge.clone());
                println!("harvest: {number}/{total}: {merge}: inert, not observed");
                continue;
            }
            match run_once(&mut session, merge.as_str()) {
                Ok(outcome) => {
                    observed += 1;
                    println!(
                        "harvest: {number}/{total}: {merge}: {} (observation {}, pending {})",
                        outcome.verdict.as_str(),
                        outcome.observation_id,
                        outcome.pending_interactions
                    );
                    // The buildability horizon: a run of consecutive
                    // inconclusive verdicts at the old end of a history is a
                    // toolchain that cannot build those trees at all, and each
                    // one still costs three build attempts. Any conclusive
                    // verdict resets the run, so this stops a band, never a
                    // scattered few.
                    if outcome.verdict
                        == choir_bridge::queue::DifferentialVerdict::InconclusiveParentFailure
                    {
                        consecutive_inconclusive += 1;
                        if stop_after_inconclusive > 0
                            && consecutive_inconclusive >= stop_after_inconclusive
                        {
                            println!(
                                "harvest: stopping: {consecutive_inconclusive} consecutive inconclusive verdicts"
                            );
                            stopped_early = true;
                            break;
                        }
                    } else {
                        consecutive_inconclusive = 0;
                    }
                    // A first interaction_failure verdict earns reproduction
                    // runs, each in a FRESH worktree triple regardless of the
                    // walk's session mode: a held tree carries the previous
                    // run's untracked build output, so a stale-artifact flake
                    // would "reproduce" perfectly in it. Independent trees are
                    // what make a specimen's 6/6 mean semantic conflict rather
                    // than shared state.
                    if outcome.verdict
                        == choir_bridge::queue::DifferentialVerdict::InteractionFailure
                    {
                        let mut runs = vec![Ok(outcome)];
                        for _ in 0..choir_bridge::queue::SPECIMEN_REPRODUCTION_RUNS {
                            runs.push(choir_bridge::queue::run_differential(
                                repo,
                                merge.as_str(),
                                Path::new(runner.as_str()),
                                Path::new(command_file.as_str()),
                                state,
                            ));
                        }
                        match choir_bridge::queue::write_specimen(repo, merge, &runs, state) {
                            Ok(path) => println!(
                                "harvest: specimen recorded at {} ({} runs)",
                                path.display(),
                                runs.len()
                            ),
                            Err(error) => {
                                eprintln!("harvest: specimen for {merge} not recorded: {error}");
                            }
                        }
                    }
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("harvest: {number}/{total}: {merge}: failed: {error}");
                }
            }
        }
        let cleanup = session.map_or(Ok(()), choir_bridge::queue::DifferentialSession::close);
        // Whatever narrowed the population is written down beside the ledger,
        // because both levers change which merges the denominator counts and
        // a reader of the corpus must be able to see that without inferring
        // it from a missing oid.
        if let Err(error) = choir_bridge::queue::record_population_restrictions(
            state,
            &inert,
            stopped_early.then_some(consecutive_inconclusive),
        ) {
            eprintln!("harvest: population restrictions not recorded: {error}");
        }
        println!(
            "harvest: {observed} observed, {failed} failed, {skipped} skipped, {} inert, {total} enumerated",
            inert.len()
        );
        if let Err(error) = cleanup {
            eprintln!("harvest cleanup failed: {error}");
            std::process::exit(1);
        }
        // Nothing observed out of a non-empty corpus is a failed harvest,
        // not a quiet one — unless every merge was already in the ledger or
        // restricted out of the population, which are working outcomes.
        if observed == 0 && skipped == 0 && inert.is_empty() {
            std::process::exit(1);
        }
        return;
    }
    // `choir-bridge queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>]`
    // Queue-as-bot: speculative-train rounds. Verdict-only by default;
    // --land fast-forwards the default branch on a green train (and
    // auto-reverts if post-land CI goes red); --watch repeats rounds
    // forever, sleeping <secs> between them.
    if args.first().map(String::as_str) == Some("queue") {
        let queue_args = parse_queue_args(&args[1..]).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });
        let [app_id, pem, repo, workdir] = match queue_args.positional.as_slice() {
            [a, b, c, d] => [a, b, c, d],
            _ => {
                eprintln!(
                    "usage: choir-bridge queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>] [--ci-command <file> [--ci-runner <path>] [--speculate]] [--differential-runner <path> --differential-command <path> --differential-state <dir>]"
                );
                std::process::exit(2);
            }
        };
        loop {
            match queue_round(
                app_id,
                Path::new(pem),
                repo,
                Path::new(workdir),
                queue_args.land,
                queue_args.differential.as_ref(),
                queue_args.ci.as_ref(),
            ) {
                Ok(()) => {}
                // In watch mode a failed round (network, rate limit) is
                // logged and retried; one-shot mode exits nonzero.
                Err(e) if queue_args.watch.is_some() => {
                    eprintln!("queue round failed (will retry): {e}")
                }
                Err(e) => {
                    eprintln!("queue round failed: {e}");
                    std::process::exit(1);
                }
            }
            match queue_args.watch {
                Some(secs) => std::thread::sleep(std::time::Duration::from_secs(secs)),
                None => break,
            }
        }
        return;
    }
    let [upstream, mirror, api_base, key_file, label] = match args.as_slice() {
        [a, b, c, d, e, rest @ ..] if rest.iter().all(|r| r == "--once") => {
            [a.clone(), b.clone(), c.clone(), d.clone(), e.clone()]
        }
        _ => {
            eprintln!(
                "usage: choir-bridge <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]"
            );
            std::process::exit(2);
        }
    };
    let once = args.iter().any(|a| a == "--once");
    let mirror = std::path::PathBuf::from(mirror);

    let key = load_or_create_key(&key_file);

    loop {
        let started = std::time::Instant::now();
        match sync_once(&upstream, &mirror, &api_base, &key, &label) {
            Ok((set, deleted, unchanged)) => eprintln!(
                "bridge: {label}: {set} set, {deleted} deleted, {unchanged} unchanged in {:?}",
                started.elapsed()
            ),
            // Transient (network, rate limit): log and retry next round.
            Err(e) => eprintln!("bridge: {label}: sync failed (will retry): {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_ops_carry_both_channel_spellings() {
        let key = ActorKey::from_secret_bytes(&[19; 32]);
        let op = ViewOp::new(choir_view::OpKind::RecordProvenance {
            subject: "repo/shared".to_string(),
            kind: "plan".to_string(),
            body: "test".to_string(),
        });
        let body = signed_op(&key, "operator/bridge", op);

        assert_eq!(body["channel"], "operator/bridge");
        assert_eq!(body["workspace"], body["channel"]);
    }

    /// `--ci-runner` alone would hand a helper no command, and the
    /// round would report an outage: our fault, for what is actually an
    /// invocation error. Refused at parse time instead.
    /// `--speculate` is a per-change verdict, and only an executor we
    /// drive can produce one. Without a command there is nothing to
    /// drive, and the round would silently fall back to the train path
    /// -- the operator asking for one thing and getting another.
    #[test]
    fn speculating_needs_a_command_to_speculate_with() {
        let base = ["1", "key", "owner/repo", "work"].map(str::to_string);
        let mut alone = base.to_vec();
        alone.push("--speculate".to_string());
        assert!(parse_queue_args(&alone).is_err());

        let mut together = base.to_vec();
        together.extend(["--ci-command", "ci.json", "--speculate"].map(str::to_string));
        let ci = parse_queue_args(&together)
            .expect("a command and speculation together")
            .ci
            .expect("ci mode is on");
        assert!(ci.speculate);

        let mut command_only = base.to_vec();
        command_only.extend(["--ci-command", "ci.json"].map(str::to_string));
        assert!(
            !parse_queue_args(&command_only)
                .expect("a bare command")
                .ci
                .expect("ci mode is on")
                .speculate,
            "the train path stays the default"
        );
    }

    #[test]
    fn a_ci_runner_needs_a_command_to_run() {
        let base = ["1", "key", "owner/repo", "work"].map(str::to_string);
        let mut both = base.to_vec();
        both.extend(["--ci-command", "ci.json", "--ci-runner", "vm"].map(str::to_string));
        let parsed = parse_queue_args(&both).expect("command and runner together");
        let ci = parsed.ci.expect("ci mode is on");
        assert_eq!(ci.command, PathBuf::from("ci.json"));
        assert_eq!(ci.runner, Some(PathBuf::from("vm")));

        let mut command_only = base.to_vec();
        command_only.extend(["--ci-command", "ci.json"].map(str::to_string));
        let parsed = parse_queue_args(&command_only).expect("a command with no runner is local");
        assert_eq!(parsed.ci.expect("ci mode is on").runner, None);

        let mut runner_only = base.to_vec();
        runner_only.extend(["--ci-runner", "vm"].map(str::to_string));
        assert!(parse_queue_args(&runner_only).is_err());

        // And no CI flags at all is the forge path, unchanged.
        assert_eq!(parse_queue_args(&base).expect("bare queue").ci, None);
    }

    #[test]
    fn differential_queue_mode_requires_all_explicit_paths() {
        let base = ["1", "key", "owner/repo", "work"].map(str::to_string);
        let mut complete = base.to_vec();
        complete.extend(
            [
                "--differential-runner",
                "runner",
                "--differential-command",
                "command.json",
                "--differential-state",
                "state",
            ]
            .map(str::to_string),
        );
        let parsed = parse_queue_args(&complete).expect("complete differential options");
        assert_eq!(parsed.positional, base);
        assert_eq!(parsed.differential.unwrap().runner, PathBuf::from("runner"));

        let mut incomplete = base.to_vec();
        incomplete.extend(
            [
                "--differential-runner",
                "runner",
                "--differential-state",
                "state",
            ]
            .map(str::to_string),
        );
        assert!(parse_queue_args(&incomplete).is_err());
    }
}
