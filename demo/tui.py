#!/usr/bin/env python3
"""Two panes, one terminal: the same twenty agents against git alone and
against a choir node.

    python3 demo/tui.py --bin <dir-with-choir-node-and-choir> [options]

    --auto [SECS]   advance beats by itself, SECS apart (default 6)
    --dump          no screen: play every beat and print both panes as text
    --port N        the node's loopback port (default 8447)
    --agents N      how many agents (default 20)

Keys while playing: Enter or Space next beat, a toggle autoplay, q quit.

Left pane: worktrees, branches, a careful maintainer merging in order
and running the tests after each merge. Right pane: the same branches
proposed to a choir node and landed by one queue round. Nothing on
either side is mocked; every line is the output of git, curl, or the
node. demo/run.sh builds the binaries and starts this.
"""

import argparse
import contextlib
import io
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import show  # noqa: E402  (the same trimming the linear script uses)

REPO = "acme/app.git"

# ---- styles ------------------------------------------------------------------
NORMAL, PROMPT, SAY, NOTE, OK, BAD, HEAD, KEY = range(8)


def typed(argv, root=None):
    """The argv as a person would type it: run dir elided, spaces quoted."""
    out = []
    for a in map(str, argv):
        if root:
            a = a.replace(str(root) + "/", "")
        if " " in a:
            a = f'"{a}"'
        out.append(a)
    return " ".join(out)


# ---- the two panes ---------------------------------------------------------
class Pane:
    def __init__(self, ui, side, title):
        self.ui, self.side, self.title = ui, side, title
        self.lines = []  # (text, style)
        self.status = ""  # one live line shown under the title (counters)

    def put(self, text="", style=NORMAL):
        with self.ui.lock:
            for line in str(text).split("\n"):
                self.lines.append((line, style))
            self.ui.dirty = True
        self.ui.transcript(f"{self.side}│ {text}")
        if self.ui.dump:
            print(f"{self.side}│ {text}", flush=True)

    def say(self, text):
        self.put(f"# {text}", SAY)

    def note(self, text):
        self.put(text, NOTE)

    def ok(self, text):
        self.put(text, OK)

    def bad(self, text):
        self.put(text, BAD)

    def set_status(self, text):
        with self.ui.lock:
            self.status = text
            self.ui.dirty = True

    def cmd(self, argv, cwd=None, shown=True, quiet=False, env=None, label=None):
        """Print the command as typed, run it, stream its output. Returns (rc, output)."""
        root = self.ui.env.run
        if shown:
            where = (label or Path(cwd).name + " ") if cwd else ""
            self.put(f"{where}$ {typed(argv, root)}", PROMPT)
        proc = subprocess.run(
            list(map(str, argv)),
            cwd=cwd,
            env=env or self.ui.env.git_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        out = proc.stdout.replace(str(root) + "/", "")
        if not quiet:
            for line in out.rstrip("\n").split("\n"):
                if line.strip():
                    self.put(line)
        return proc.returncode, out


# ---- the environment: paths, node, git ---------------------------------------
class Env:
    def __init__(self, bin_dir, port, agents):
        self.bin = Path(bin_dir)
        self.port = port
        self.api = f"http://127.0.0.1:{port}"
        self.url = f"{self.api}/{REPO}"
        self.agents = agents
        self.run = HERE / ".run"
        self.left = self.run / "left"
        self.right = self.run / "right"
        self.node = None
        self.git_env = dict(os.environ)
        self.git_env.update(
            {
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_AUTHOR_NAME": "agent",
                "GIT_COMMITTER_NAME": "agent",
                "GIT_AUTHOR_EMAIL": "agent",
                "GIT_COMMITTER_EMAIL": "agent",
                "GIT_CONFIG_COUNT": "5",
                "GIT_CONFIG_KEY_0": "commit.gpgsign",
                "GIT_CONFIG_VALUE_0": "false",
                "GIT_CONFIG_KEY_1": "init.defaultBranch",
                "GIT_CONFIG_VALUE_1": "main",
                "GIT_CONFIG_KEY_2": "merge.conflictStyle",
                "GIT_CONFIG_VALUE_2": "diff3",
                "GIT_CONFIG_KEY_3": "rerere.enabled",
                "GIT_CONFIG_VALUE_3": "false",
                "GIT_CONFIG_KEY_4": "advice.detachedHead",
                "GIT_CONFIG_VALUE_4": "false",
            }
        )

    def names(self):
        return [f"agent-{i:02d}" for i in range(1, self.agents + 1)]

    # -- process plumbing --
    def git(self, cwd, *args, check=True):
        p = subprocess.run(
            ["git", *args], cwd=cwd, env=self.git_env, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, check=False,
        )
        if check and p.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} in {cwd}: {p.stdout}")
        return p

    def choir(self, *args):
        return subprocess.run(
            [str(self.bin / "choir"), *args], env=self.git_env, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, check=False,
        )

    def get(self, path):
        with urllib.request.urlopen(f"{self.api}{path}", timeout=30) as r:
            return json.loads(r.read())

    def get_text(self, path):
        with urllib.request.urlopen(f"{self.api}{path}", timeout=30) as r:
            return r.read().decode()

    def post(self, path, body):
        req = urllib.request.Request(
            f"{self.api}{path}", data=json.dumps(body).encode(), method="POST"
        )
        with urllib.request.urlopen(req, timeout=600) as r:
            return json.loads(r.read())

    def healthy(self):
        try:
            urllib.request.urlopen(f"{self.api}/healthz", timeout=1)
            return True
        except (urllib.error.URLError, OSError):
            return False

    # -- lifecycle --
    def reset(self):
        # One take at a time: they share .run/, and a second one wiping it
        # mid-beat shows up as a cascade of missing files on the first.
        take = self.run / "take.pid"
        if take.exists():
            try:
                os.kill(int(take.read_text()), 0)
                sys.exit(f"another take is running (pid {take.read_text()}); press q there first")
            except (ValueError, ProcessLookupError):
                pass
            except PermissionError:
                sys.exit(f"another take is running (pid {take.read_text()}); press q there first")
        pidfile = self.run / "node.pid"
        if pidfile.exists():
            try:
                os.kill(int(pidfile.read_text()), signal.SIGTERM)
                time.sleep(0.2)
            except (ValueError, ProcessLookupError):
                pass
        if self.healthy():
            sys.exit(f"something else is listening on {self.api}; pass --port")
        shutil.rmtree(self.run, ignore_errors=True)
        for d in (self.run / "root", self.run / "queue", self.run / "ci", self.left / "ci", self.right):
            d.mkdir(parents=True)
        take.write_text(str(os.getpid()))

    def start_node(self):
        keys = self.run / "trusted-keys"
        keys.write_text(self.choir("key", str(self.run / "operator.key"), "acme/operator").stdout)
        for side in (self.run, self.left):
            runner = side / "ci" / "run-tests"
            runner.write_text("#!/bin/sh\nexec ./test.sh\n")
            runner.chmod(0o755)
        (self.run / "ci-command.json").write_text(json.dumps({
            "format_version": 1, "program": str(self.run / "ci" / "run-tests"),
            "args": [], "timeout_seconds": 60,
        }))
        log = open(self.run / "node.log", "w")
        self.node = subprocess.Popen(
            [str(self.bin / "choir-node"), str(self.run / "root"), str(self.port),
             "--create", REPO, "--keys-file", str(keys),
             "--ci-command", str(self.run / "ci-command.json"),
             "--queue-tree", str(self.run / "queue")],
            stdout=log, stderr=log, env=self.git_env,
        )
        (self.run / "node.pid").write_text(str(self.node.pid))
        for _ in range(200):
            if self.healthy():
                break
            time.sleep(0.05)
        else:
            sys.exit(f"node did not come up; see {self.run / 'node.log'}")
        (self.run / "node.pub").write_text(
            self.choir("key", str(self.run / "root" / ".choir" / "node.key"), "node").stdout
        )

    def stop_node(self):
        if self.node and self.node.poll() is None:
            self.node.terminate()

    # -- trimmed readers, shared with the linear script --
    def shown(self, what, payload):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            if what == "conflict":
                show.conflict(payload)
            else:
                getattr(show, what)(payload)
        return buf.getvalue().replace(str(self.run) + "/", "").rstrip("\n").split("\n")

    def next_seq(self):
        return self.get("/api/view")["log"]["next_seq"]

    def queue_run(self):
        return self.post("/api/queue/run", {"repo": REPO, "branch": "main"})


# ---- the screen ------------------------------------------------------------
class UI:
    def __init__(self, env, dump=False, auto=None):
        self.env = env
        self.dump = dump
        self.auto = auto
        self.lock = threading.Lock()
        self.dirty = True
        self.left = Pane(self, "L", "git: worktrees, branches, a careful maintainer")
        self.right = Pane(self, "R", "choir: the same branches, one round")
        self.beat_no, self.beat_total, self.beat_title = 0, 0, ""
        self.narration = ""
        self.hint = ""
        self.advance = threading.Event()
        self.quit = False
        self.done = False
        self.beat_started = time.monotonic()
        self.started = time.monotonic()
        # Every line either pane shows, with its time, so a take that looked
        # wrong can be read afterwards: .run/take.log.
        self.log = open(env.run / "take.log", "a")
        self.log_lock = threading.Lock()
        self.transcript(f"take: pid {os.getpid()}, {'dump' if dump else 'screen'}, auto {auto}")

    def transcript(self, line):
        with self.log_lock:
            self.log.write(f"{time.monotonic() - self.started:7.2f}  {line}\n")
            self.log.flush()

    def beat(self, no, title, narration):
        with self.lock:
            self.beat_no, self.beat_title, self.narration = no, title, narration
            self.beat_started = time.monotonic()
            self.dirty = True
        self.transcript(f"== beat {no}: {title} == {narration}")
        for pane in (self.left, self.right):
            pane.put("", NORMAL)
            pane.put(f"── beat {no}: {title}", HEAD)
        if self.dump:
            print(f"\n== beat {no}: {title} ==\n#  {narration}", flush=True)

    def wait(self):
        """Between beats: wait for Enter, or for the autoplay delay."""
        if self.dump:
            return
        self.advance.clear()
        if self.auto is not None:
            with self.lock:
                self.hint = f"autoplay: next beat in {self.auto:.0f}s   a stop   q quit"
                self.dirty = True
            self.advance.wait(self.auto)
        else:
            with self.lock:
                self.hint = "[Enter] next beat   a autoplay   q quit"
                self.dirty = True
            self.advance.wait()
        with self.lock:
            self.hint = ""
            self.dirty = True
        if self.quit:
            raise SystemExit

    # -- curses --
    def run(self, play):
        if self.dump:
            play(self)
            return
        import curses

        curses.wrapper(self._main, play)

    def _main(self, stdscr, play):
        import curses

        curses.curs_set(0)
        stdscr.nodelay(True)
        stdscr.keypad(True)
        curses.use_default_colors()
        palette = {
            NORMAL: (-1, -1, 0), PROMPT: (-1, -1, curses.A_BOLD), SAY: (3, -1, 0),
            NOTE: (-1, -1, curses.A_DIM), OK: (2, -1, 0), BAD: (1, -1, curses.A_BOLD),
            HEAD: (6, -1, curses.A_BOLD), KEY: (-1, -1, curses.A_REVERSE),
        }
        self.attrs = {}
        for i, (style, (fg, bg, extra)) in enumerate(palette.items(), start=1):
            curses.init_pair(i, fg, bg)
            self.attrs[style] = curses.color_pair(i) | extra

        h, w = stdscr.getmaxyx()
        self.transcript(f"screen: {w}x{h}, TERM {os.environ.get('TERM', '?')}, colors {curses.COLORS}")
        worker = threading.Thread(target=self._play, args=(play,), daemon=True)
        worker.start()
        while True:
            key = stdscr.getch()
            if key != -1:
                self.transcript(f"key: {key} in beat {self.beat_no}")
            if key in (ord("q"), ord("Q")):
                self.quit = True
                self.advance.set()
                break
            if key in (10, 13, ord(" "), curses.KEY_ENTER):
                self.advance.set()
            if key in (ord("a"), ord("A")):
                self.auto = None if self.auto is not None else 6.0
                self.advance.set()
            if key == curses.KEY_RESIZE:
                h, w = stdscr.getmaxyx()
                self.transcript(f"screen: resized to {w}x{h}")
                self.dirty = True
            if self.dirty or int(time.monotonic() * 2) % 2 == 0:
                self._draw(stdscr)
            time.sleep(0.04)

    def _play(self, play):
        try:
            play(self)
        except SystemExit:
            pass
        except Exception as e:  # show it on screen rather than dying silently
            self.left.bad(f"demo error: {e!r}")
        with self.lock:
            self.done = True
            self.hint = "done   q quit"
            self.dirty = True

    def _draw(self, stdscr):
        import curses

        with self.lock:
            self.dirty = False
            h, w = stdscr.getmaxyx()
            stdscr.erase()
            if h < 24 or w < 90:
                stdscr.addstr(0, 0, f"need at least 90x24; this terminal is {w}x{h}")
                stdscr.refresh()
                return
            A = self.attrs

            def text(y, x, s, attr=0, width=None):
                width = width if width is not None else w - x
                if width <= 0 or y >= h:
                    return
                s = s[: width - 1] + "…" if len(s) > width else s
                try:
                    stdscr.addstr(y, x, s, attr)
                except curses.error:
                    pass

            # title bar
            title = f" choir · demo   beat {self.beat_no}/{self.beat_total}: {self.beat_title}"
            elapsed = time.monotonic() - self.beat_started
            right = f"{elapsed:5.1f}s " if not self.done else " done "
            bar = title + " " * max(0, w - len(title) - len(right)) + right
            text(0, 0, bar[:w], A[KEY], w)
            # narration
            text(1, 1, self.narration, A[SAY])
            # panes
            split = w // 2
            lw, rw = split - 1, w - split - 1
            top, bottom = 3, h - 2
            text(2, 1, self.left.title, A[HEAD], lw - 1)
            text(2, split + 1, self.right.title, A[HEAD], rw - 1)
            for y in range(2, bottom):
                text(y, split, "│", A[NOTE], 1)
            for pane, x, pw in ((self.left, 1, lw - 1), (self.right, split + 1, rw - 1)):
                rows = bottom - top - (1 if pane.status else 0)
                for i, (line, style) in enumerate(pane.lines[-rows:]):
                    text(top + i, x, line, A[style], pw)
                if pane.status:
                    text(bottom - 1, x, pane.status, A[KEY], pw)
            # footer
            text(h - 1, 1, self.hint, A[NOTE])
            stdscr.refresh()


# ---- helpers the beats share -------------------------------------------------
def ms(t0):
    return f"{(time.monotonic() - t0) * 1000:.0f} ms"


def seed_tree(path, agents):
    (path / "config.toml").write_text('greeting = "hello"\n')
    test = path / "test.sh"
    test.write_text(
        "#!/bin/sh\n# the repo's own test: the greeting must be a quoted word\n"
        "grep -q '^greeting = \"[a-z]*\"$' config.toml\n"
    )
    test.chmod(0o755)
    (path / "svc").mkdir()
    for name in agents:
        (path / "svc" / f"{name}.toml").write_text("enabled = false\n")


def agent_change(wt, name):
    """What agent <name> does in its worktree. 03 and 07 also touch THE line."""
    (wt / "svc" / f"{name}.toml").write_text("enabled = true\n")
    files = f"svc/{name}.toml"
    if name.endswith("-03"):
        (wt / "config.toml").write_text('greeting = "hola"\n')
        files += ", config.toml"
    if name.endswith("-07"):
        (wt / "config.toml").write_text('greeting = "bonjour"\n')
        files += ", config.toml"
    return files


def run_ci(runner, cwd, env):
    """The maintainer's CI step: a shell runs the runner, so a runner that
    cannot start is exit 126, the way any CI script would see it."""
    return subprocess.run(["/bin/sh", "-c", f'exec "{runner}"'], cwd=cwd, env=env.git_env,
                          capture_output=True, check=False).returncode


def round_names(env):
    """Round-local proposal ids are positions in the sorted refnames."""
    prefix = f"{REPO}:refs/for/main/"
    refs = sorted(k for k in env.get("/api/view")["refs"] if k.startswith(prefix))
    return {i + 1: r[len(prefix):] for i, r in enumerate(refs)}


def compact(names):
    """agent-01..06, agent-08..20: runs of numbered names, folded."""
    out, run = [], []

    def flush():
        if len(run) > 2:
            out.append(f"{run[0]}..{run[-1].rsplit('-', 1)[-1]}")
        else:
            out.extend(run)
        run.clear()

    for n in names:
        stem, _, num = n.rpartition("-")
        prev = run[-1].rpartition("-") if run else None
        if run and prev[0] == stem and num.isdigit() and prev[2].isdigit() and int(num) == int(prev[2]) + 1:
            run.append(n)
        else:
            flush()
            run.append(n)
    flush()
    return ", ".join(out)


def round_lines(env, r, names):
    """The round's answer with proposals named rather than numbered."""
    name = lambda i: names.get(i, str(i)).split("/")[0]  # noqa: E731
    yield f"  landed   : {compact([name(i) for i in r['merged']]) or '-'}  ({len(r['merged'])})"
    evicted = ", ".join(f"{name(x['id'])} ({x['why']})" for x in r["rejected"])
    yield f"  evicted  : {evicted or '-'}"
    if r.get("already_integrated"):
        yield f"  skipped  : {len(r['already_integrated'])} already on main"
    yield f"  stalled  : {(r['stalled'] or '-').replace(str(env.run) + '/', '')}"
    yield f"  main tip : {r['tip'][:7]}"


def parallel(fns):
    """Run both panes' work at once; the first failure stops the take."""
    errors = []

    def guarded(f):
        try:
            f()
        except BaseException as e:  # re-raised in the caller, once
            errors.append(e)

    threads = [threading.Thread(target=guarded, args=(f,), daemon=True) for f in fns]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if errors:
        raise errors[0]


# ---- beats -------------------------------------------------------------------
def beat0(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(0, "two remotes, one base", "left: a bare repo, fast-forward only. right: a choir node. Same seed, same test.sh, same twenty agents.")
    L.cmd(["git", "init", "-q", "--bare", "origin.git"], cwd=env.left, label="left ")
    L.cmd(["git", "-C", "origin.git", "config", "receive.denyNonFastForwards", "true"], cwd=env.left, label="left ")
    L.note("  every push is a compare-and-swap; nobody can overwrite anybody")
    R.note(f"  choir-node on {env.api}, one repo {REPO}, queue runs the repo's test.sh")
    R.cmd(["curl", "-s", "-o", "/dev/null", "-w", "healthz %{http_code}\\n", f"{env.api}/healthz"], label="right ")

    def seed(pane, side, url):
        seed_dir = side / "seed"
        env.git(side, "clone", "-q", url, "seed")
        seed_tree(seed_dir, env.names())
        env.git(seed_dir, "add", ".")
        env.git(seed_dir, "commit", "-q", "-m", "base: greeting, test.sh, twenty services")
        env.git(seed_dir, "push", "-q", "origin", "HEAD:main")
        pane.put(f"seed $ git push origin HEAD:main", PROMPT)
        pane.note("  base: config.toml, test.sh, svc/agent-01..20.toml")

    parallel([lambda: seed(L, env.left, str(env.left / "origin.git")), lambda: seed(R, env.right, env.url)])
    ui.wait()


def beat1(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(1, "twenty agents, twenty worktrees, twenty branches", "each agent enables its service on its own branch. Two of them (03, 07) also change the greeting line. Watch: neither side rejects a push; isolation is free.")
    L.put("each $ git worktree add wt/<agent> -b <agent> main && edit && git commit && git push origin <agent>", PROMPT)
    R.put("each $ git worktree add wt/<agent> -b <agent> main && edit && git commit && git push origin HEAD:refs/for/main/<agent>/enable", PROMPT)
    counters = {"L": [0, 0], "R": [0, 0]}  # pushed, rejected

    def work(pane, key, side, refspec):
        seed = side / "seed"
        t0 = time.monotonic()
        # `git worktree add` scans .git/worktrees/* while a sibling is still
        # writing its own entry, so creation is serialised per repo; the
        # edits, commits and pushes below stay concurrent.
        adding = threading.Lock()

        def one(name):
            wt = side / "wt" / name
            with adding:
                env.git(seed, "worktree", "add", "-q", str(wt), "-b", name, "main")
            files = agent_change(wt, name)
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-q", "-m", f"{name}: enable")
            p = env.git(wt, "push", "-q", "origin", refspec(name), check=False)
            with ui.lock:
                counters[key][0 if p.returncode == 0 else 1] += 1
            pane.put(f"  {name}  {'pushed' if p.returncode == 0 else 'REJECTED'}  ({files})", OK if p.returncode == 0 else BAD)
            pane.set_status(f" pushed {counters[key][0]}  rejected {counters[key][1]}  {ms(t0)}")

        parallel([lambda n=n: one(n) for n in env.names()])
        pane.set_status("")
        pane.put(f"  {counters[key][0]} pushed, {counters[key][1]} rejected, {ms(t0)}", NOTE)

    parallel([
        lambda: work(L, "L", env.left, lambda n: n),
        lambda: work(R, "R", env.right, lambda n: f"HEAD:refs/for/main/{n}/enable"),
    ])
    ui.wait()


def beat2(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(2, "integration: twenty branches become one main", "left: the maintainer merges in order and runs the tests after each merge. right: one queue round. Watch what happens at agent-07, and what happens to 08 to 20.")
    stats = {}

    def left():
        m = env.left / "maintainer"
        env.git(env.left, "clone", "-q", str(env.left / "origin.git"), "maintainer")
        L.put("maintainer $ for b in agent-01..20: git merge --no-ff origin/$b && ci/run-tests || skip", PROMPT)
        t0 = time.monotonic()
        merged, blocked, ci = [], [], 0
        env.git(m, "fetch", "-q", "origin")
        for name in env.names():
            t1 = time.monotonic()
            p = env.git(m, "merge", "-q", "--no-ff", "-m", f"merge {name}", f"origin/{name}", check=False)
            if p.returncode != 0:
                conflict = [l.split(" in ", 1)[-1] for l in p.stdout.splitlines() if l.startswith("CONFLICT")]
                env.git(m, "merge", "--abort")
                blocked.append(name)
                L.bad(f"  merge {name}  CONFLICT {', '.join(conflict)}  → skipped; author paged, branch waits")
                continue
            ci += 1
            rc = run_ci(env.left / "ci" / "run-tests", m, env)
            if rc == 0:
                merged.append(name)
                L.put(f"  merge {name}  ✓ tests ✓  {ms(t1)}", OK)
            else:
                env.git(m, "reset", "-q", "--hard", "HEAD~1")
                blocked.append(name)
                L.bad(f"  merge {name}  tests ✗ (exit {rc})  → reverted; author paged")
            L.set_status(f" merged {len(merged)}  blocked {len(blocked)}  ci runs {ci}  {ms(t0)}")
        env.git(m, "push", "-q", "origin", "main")
        L.set_status("")
        stats["L"] = (len(merged), len(blocked), ci, ms(t0))
        L.put(f"  {len(merged)} merged, {len(blocked)} blocked, {ci} CI runs one after another, {stats['L'][3]}", NOTE)
        L.note("  agent-07's work is on a branch nobody else can see on main until its author comes back")

    def right():
        t0 = time.monotonic()
        R.put("$ curl -X POST /api/queue/run  {repo, branch: main}", PROMPT)
        R.set_status(" one round running: speculative train of 20, CI per candidate, in parallel")
        names = round_names(env)
        r = env.queue_run()
        R.set_status("")
        for line in round_lines(env, r, names):
            R.put(line, BAD if "Conflict" in line else NORMAL)
        lag = env.get("/api/view")["sequencer_lag"]
        stats["R"] = (len(r["merged"]), len(r["rejected"]), env.agents, ms(t0))
        R.put(f"  {len(r['merged'])} landed, {len(r['rejected'])} evicted, {env.agents} CI runs in one train, {stats['R'][3]}", NOTE)
        R.note(f"  sequencer decision latency since start: p50 {lag['decision']['p50_us']} us, p99 {lag['decision']['p99_us']} us over {lag['observed_ops']} ops")
        R.note("  agent-07 was a verdict in the round, not a stop: evicted first-class, 08 to 20 landed behind it")

    parallel([left, right])
    ui.wait()


def beat3(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(3, "the conflict: a branch that waits, or a value on main", "left: agent-12 needs agent-07's change and can only get it by taking on 07's conflict. right: 07 lands the conflict on main as-is, the node reads it three-sided, 12 builds on top, 07 resolves later.")

    def left():
        wt = env.left / "wt" / "agent-12"
        L.say("agent-12 needs what agent-07 did. On a branch, that means merging agent-07's branch, conflict included.")
        env.git(wt, "fetch", "-q", "origin")
        env.git(wt, "merge", "-q", "origin/main", check=False)
        rc, _ = L.cmd(["git", "merge", "origin/agent-07"], cwd=wt, quiet=True)
        L.bad("  CONFLICT (content): Merge conflict in config.toml" if rc else "  merged")
        L.cmd(["git", "merge", "--abort"], cwd=wt)
        L.note("  agent-12 waits for agent-07, or resolves somebody else's conflict in its own worktree")
        L.note("  main does not know a conflict exists; a forge would show 07's PR as 'has conflicts'")

    def right():
        wt07, wt12 = env.right / "wt" / "agent-07", env.right / "wt" / "agent-12"
        R.say("agent-07 pulls main, gets CONFLICT as anywhere, commits it as-is, and pushes it. Accepted.")
        rc, out = R.cmd(["git", "pull", "--no-rebase", "-q", "origin", "main"], cwd=wt07, quiet=True)
        for line in out.splitlines():
            if line.startswith("CONFLICT"):
                R.bad("  " + line)
        R.cmd(["git", "add", "config.toml"], cwd=wt07)
        R.cmd(["git", "commit", "-q", "-m", "merge main: conflict kept as a value"], cwd=wt07)
        rc, out = R.cmd(["git", "push", "origin", "HEAD:main"], cwd=wt07, quiet=True)
        for line in out.splitlines():
            if "->" in line or "rejected" in line:
                R.put("  " + line.strip(), OK if rc == 0 else BAD)
        env.git(wt07, "push", "-q", "origin", ":refs/for/main/agent-07/enable")
        R.say("the node reads that commit as what it is:")
        html = env.get_text(f"/r/{REPO[:-4]}/blob/main/config.toml")
        for line in env.shown("conflict", html):
            R.put(line, OK)
        R.say("agent-12 pulls main, gets the conflict commit, does its own work on top, lands.")
        env.git(wt12, "pull", "--no-rebase", "-q", "origin", "main")
        (wt12 / "svc" / "agent-12.toml").write_text("enabled = true\nreplicas = 2\n")
        R.cmd(["git", "commit", "-q", "-am", "agent-12: replicas (on top of the unresolved conflict)"], cwd=wt12)
        rc, out = R.cmd(["git", "push", "origin", "HEAD:main"], cwd=wt12, quiet=True)
        for line in out.splitlines():
            if "->" in line or "rejected" in line:
                R.put("  " + line.strip(), OK if rc == 0 else BAD)
        R.say("agent-07 comes back and resolves. One more commit; the history keeps the conflict.")
        env.git(wt07, "pull", "--no-rebase", "-q", "origin", "main")
        (wt07 / "config.toml").write_text('greeting = "hola"\n')
        R.cmd(["git", "commit", "-q", "-am", "resolve: greeting is Spanish"], cwd=wt07)
        env.git(wt07, "push", "-q", "origin", "HEAD:main")
        R.cmd(["git", "log", "--oneline", "-4", "origin/main"], cwd=wt07)

    parallel([left, right])
    ui.wait()


def beat4(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(4, "what each side can prove afterwards", "left: a commit graph, which is honest and good. right: every landing, check verdict and ref move as one signed hash chain, verified offline with the node's public key.")

    def left():
        m = env.left / "maintainer"
        L.cmd(["git", "log", "--oneline", "-3", "main"], cwd=m)
        L.note("  proof of order: parent pointers. signatures on merges: none configured. CI verdicts: not recorded.")
        L.note("  who merged what, in what order, and what the tests said lives in the maintainer's terminal history")

    def right():
        entries = env.get(f"/api/log?from=0")["entries"]
        kinds = {}
        for e in entries:
            k = next(iter(json.loads(bytes.fromhex(e["payload_hex"]))["kind"]))
            kinds[k] = kinds.get(k, 0) + 1
        R.put(f"$ choir log {env.api}   ({len(entries)} entries: " + ", ".join(f"{v} {k}" for k, v in sorted(kinds.items())) + ")", PROMPT)
        for line in env.shown("log", {"entries": entries[-12:]})[-8:]:
            R.put(line)
        R.put(f"$ choir log {env.api} --verify --keys node.pub", PROMPT)
        p = env.choir("log", env.api, "--verify", "--keys", str(env.run / "node.pub"))
        R.put("  " + p.stderr.strip().splitlines()[-1], OK)
        R.note("  continuity, every hash recomputed, every signature checked, with nothing but the wire format and one key")

    parallel([left, right])
    ui.wait()


def beat5(ui):
    env, L, R = ui.env, ui.left, ui.right
    ui.beat(5, "a broken test runner is not a failing test", "two more branches: agent-05 breaks the test, agent-09 adds a note. Then the CI runner on each side loses its exec bit. Watch who gets blamed, and what happens once the runner is back.")

    def prepare(side, refspec):
        seed = side / "seed"
        env.git(seed, "fetch", "-q", "origin")
        for name, edit in (("agent-05", lambda wt: (wt / "config.toml").write_text("greeting = hi\n")),
                           ("agent-09", lambda wt: (wt / "NOTES.md").write_text("Run ./test.sh before you push.\n"))):
            wt = side / "wt" / name
            env.git(wt, "checkout", "-q", "--detach", "origin/main")
            env.git(wt, "checkout", "-q", "-b", f"{name}-2")
            edit(wt)
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-q", "-m", f"{name}: {'unquoted greeting (breaks test.sh)' if name.endswith('05') else 'notes'}")
            env.git(wt, "push", "-q", "origin", refspec(name))

    parallel([lambda: prepare(env.left, lambda n: f"HEAD:{n}-2"),
              lambda: prepare(env.right, lambda n: f"HEAD:refs/for/main/{n}/second")])
    for pane, runner in ((L, env.left / "ci" / "run-tests"), (R, env.run / "ci" / "run-tests")):
        pane.put(f"$ chmod -x {typed([runner], env.run)}", PROMPT)
        runner.chmod(0o644)

    def left(broken):
        m = env.left / "maintainer"
        env.git(m, "fetch", "-q", "origin")
        for name in ("agent-05", "agent-09"):
            env.git(m, "merge", "-q", "--no-ff", "-m", f"merge {name}", f"origin/{name}-2", check=False)
            rc = run_ci(env.left / "ci" / "run-tests", m, env)
            if rc == 0:
                L.put(f"  merge {name}  ✓ tests ✓", OK)
            else:
                env.git(m, "reset", "-q", "--hard", "HEAD~1")
                L.bad(f"  merge {name}  tests ✗ (exit {rc})  → reverted; author paged")
        if broken:
            L.note("  exit 126 is 'could not run', but red is red to a script: both authors paged for a runner they do not own")

    def right(broken):
        R.put("$ curl -X POST /api/queue/run", PROMPT)
        names = round_names(env)
        r = env.queue_run()
        for line in round_lines(env, r, names):
            R.put(line, BAD if ("Conflict" in line or "CiFailure" in line or "could not" in line) else NORMAL)
        if broken:
            R.note("  Errored is a statement about us: both requeued, nothing evicted, main unmoved, reason recorded")
        else:
            R.note("  Failed is a statement about the change: only agent-05 pays; agent-09 landed in the same round")

    parallel([lambda: left(True), lambda: right(True)])
    for pane, runner in ((L, env.left / "ci" / "run-tests"), (R, env.run / "ci" / "run-tests")):
        pane.put(f"$ chmod +x {typed([runner], env.run)}   # the runner is back", PROMPT)
        runner.chmod(0o755)
    parallel([lambda: left(False), lambda: right(False)])
    L.note("  the honest summary: for disjoint work at a low conflict rate, branches and a merge queue are fine.")
    R.note("  the claim is what happens at the conflict, at the infra failure, and in what you can prove after.")
    ui.wait()


BEATS = [beat0, beat1, beat2, beat3, beat4, beat5]


def play(ui):
    ui.beat_total = len(BEATS) - 1
    for beat in BEATS:
        if ui.quit:
            break
        beat(ui)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, help="directory holding choir-node and choir")
    ap.add_argument("--auto", nargs="?", const=6.0, type=float, default=None)
    ap.add_argument("--dump", action="store_true")
    ap.add_argument("--port", type=int, default=8447)
    ap.add_argument("--agents", type=int, default=20)
    args = ap.parse_args()
    env = Env(args.bin, args.port, args.agents)
    for b in ("choir-node", "choir"):
        if not (env.bin / b).exists():
            sys.exit(f"no {b} in {env.bin}; run demo/run.sh")
    env.reset()
    env.start_node()
    ui = UI(env, dump=args.dump, auto=args.auto)
    try:
        ui.run(play)
    except Exception as e:  # text mode: one line, then stop; the screen shows its own
        sys.exit(f"demo error: {e}\nnode log: {env.run / 'node.log'}")
    finally:
        env.stop_node()


if __name__ == "__main__":
    main()
