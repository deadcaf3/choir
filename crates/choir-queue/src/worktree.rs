//! An executor that materializes what it is asked about (D18).
//!
//! [`crate::local::LocalRunner`] runs a command in a directory somebody
//! else prepared. That is fine for one train commit and wrong for a
//! train: [`crate::MergeQueue`] tests every member against *its own*
//! speculative state, so a batch of jobs is a batch of different trees,
//! and running them all in one checkout tests the last one N times and
//! reports the answer under N different subjects. The failure is silent
//! and every verdict is well-formed, which is the shape of a defect
//! nobody finds.
//!
//! [`Job::directory`] already spells the way out. `None` means the
//! provider materializes [`Job::subject`] itself — written for a
//! microVM, and true of anything that can turn a content address into a
//! tree. A git repository can, for the subjects
//! [`crate::git::GitSpeculator`] produces, because those are its own
//! commit ids.
//!
//! So this is the executor that closes the loop: `git worktree add
//! --detach` per job, the command in that worktree, the worktree
//! removed afterwards. A job that *does* name a directory is run there
//! untouched, because a caller who prepared a tree has said what they
//! want and the subject is then not ours to interpret.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::executor::{CiExecutor, ExecutorError, ExecutorInfo, Job, Verdict, PROTOCOL};
use crate::local::LocalRunner;

/// Runs each job in a throwaway worktree of `repo` at the job's subject.
#[derive(Debug)]
pub struct WorktreeRunner {
    repo: PathBuf,
    root: PathBuf,
    inner: LocalRunner,
}

/// Runs git in `dir`, returning stderr on failure.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

impl WorktreeRunner {
    /// An executor over `repo`, checking out under `root`.
    ///
    /// `root` is created on demand and its children are removed as each
    /// batch finishes. It must not be inside `repo`'s worktree: a
    /// checkout there would appear as untracked files to every job.
    #[must_use]
    pub fn new(repo: PathBuf, root: PathBuf) -> Self {
        Self {
            repo,
            root,
            inner: LocalRunner::new(),
        }
    }

    /// Checks out `job`'s subject and returns the job rewritten to run
    /// there, plus the path to clean up.
    fn provision(&self, job: &Job, slot: usize) -> Result<(Job, PathBuf), String> {
        let oid = job
            .subject
            .git_oid()
            .ok_or_else(|| "subject is not a git object id".to_string())?;
        std::fs::create_dir_all(&self.root).map_err(|e| format!("create checkout root: {e}"))?;
        // Slot as well as oid: two members of one train can legitimately
        // carry the same subject -- a change whose merge was a no-op --
        // and `worktree add` refuses a path that exists.
        let path = self.root.join(format!("{oid}-{slot}"));
        // A run that died between `worktree add` and the cleanup left a
        // checkout at this path, and `worktree add` refuses a path that
        // exists -- so without this, one crash makes every later batch
        // fault at provisioning until somebody cleans up by hand.
        let _ = self.discard(&path);
        let target = path
            .to_str()
            .ok_or_else(|| format!("checkout path {path:?} is not UTF-8"))?;
        git(
            &self.repo,
            &["worktree", "add", "--detach", "--quiet", target, &oid],
        )?;
        let mut prepared = job.clone();
        prepared.directory = Some(path.clone());
        Ok((prepared, path))
    }

    /// Removes a checkout, by git's bookkeeping and then by force.
    ///
    /// Both, because what is at the path is not always a worktree git
    /// knows about. Its own are removed by the first call; the debris a
    /// crashed run left is a directory git has no record of, which the
    /// first call refuses and the second deletes. Deleting alone would
    /// leave git's administrative entry pointing at nothing, which is
    /// why the order is this way round and not the other.
    fn discard(&self, path: &Path) -> Result<(), String> {
        let target = path
            .to_str()
            .ok_or_else(|| format!("checkout path {path:?} is not UTF-8"))?;
        let by_git = git(&self.repo, &["worktree", "remove", "--force", target]);
        if by_git.is_err() {
            let _ = std::fs::remove_dir_all(path);
            let _ = git(&self.repo, &["worktree", "prune"]);
        }
        Ok(())
    }
}

impl CiExecutor for WorktreeRunner {
    fn info(&mut self) -> Result<ExecutorInfo, ExecutorError> {
        Ok(ExecutorInfo {
            name: "worktree".to_string(),
            protocol: PROTOCOL,
        })
    }

    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        // Index alignment is the seam's whole contract, so a job we
        // could not check out keeps its slot with an `Errored` in it
        // rather than being dropped from the batch. `Errored` and not
        // `Failed`: a checkout we could not make is our fault, and the
        // queue must not eject an author over it.
        let mut prepared: Vec<Option<Job>> = Vec::with_capacity(jobs.len());
        let mut faults: Vec<Option<String>> = Vec::with_capacity(jobs.len());
        let mut checkouts: Vec<PathBuf> = Vec::new();
        for (slot, job) in jobs.iter().enumerate() {
            if job.directory.is_some() {
                // Somebody already prepared a tree and said so. The
                // subject is then their statement about what that tree
                // is, not an instruction to us.
                prepared.push(Some(job.clone()));
                faults.push(None);
                continue;
            }
            match self.provision(job, slot) {
                Ok((job, path)) => {
                    checkouts.push(path);
                    prepared.push(Some(job));
                    faults.push(None);
                }
                Err(why) => {
                    prepared.push(None);
                    faults.push(Some(why));
                }
            }
        }

        let batch: Vec<Job> = prepared.iter().flatten().cloned().collect();
        let result = self.inner.run(&batch);
        for path in &checkouts {
            let _ = self.discard(path);
        }
        let mut ran = result?.into_iter();

        let verdicts = prepared
            .iter()
            .zip(faults)
            .map(|(job, fault)| match (job, fault) {
                (Some(_), _) => ran.next().unwrap_or(Verdict::Errored {
                    provider: "worktree".to_string(),
                    detail: "the inner runner answered short".to_string(),
                }),
                (None, fault) => Verdict::Errored {
                    provider: "worktree".to_string(),
                    detail: fault.unwrap_or_else(|| "no checkout".to_string()),
                },
            })
            .collect();
        Ok(verdicts)
    }
}
