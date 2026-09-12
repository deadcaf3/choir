#!/usr/bin/env python3
"""Two columns, one terminal: the same twenty agents against git alone and
against a choir node, both sides running their whole script at once.

    python3 demo/tui.py --bin <dir-with-choir-node-and-choir> [options]

    --plain         no colour, no live rows: for a pipe or a file
    --port N        the node's loopback port (default 8447, or the next free one)
    --agents N      how many agents (default 20, at least 18)
    --overlap K     how many agents rewrite the greeting line (default 2)
    --ci-seconds S  how long the repo's test.sh takes (default 3): the cost
                    model of beat 2 is the maintainer paying it once per
                    merge in series, and the round paying it once

Nothing waits for a key. Every row goes to the terminal's own scrollback
as it happens, stamped with the seconds since the take began, so the two
columns read as one timeline: scroll up to see what the right side was
doing while the left was still merging. The bottom line of each column
is live while its side is working. Ctrl-C stops the take and the node.

Left column: worktrees, branches, a careful maintainer merging in order
and running the tests after each merge. Right column: the same branches
proposed to a choir node and landed by one queue round. Nothing on
either side is mocked; every row is the output of git, curl, or the
node. demo/run.sh builds the binaries and starts this. The transcript
of the last take is demo/.run/take.log.
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
import textwrap
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import show  # noqa: E402  (the same trimming the linear script uses)

REPO = "acme/app.git"
# No single git, curl or CI step may hang a take: past this it is an error
# in the column, with the command named, rather than a frozen side.
STEP_TIMEOUT = 300

# ---- styles ------------------------------------------------------------------
NORMAL, PROMPT, SAY, NOTE, OK, BAD, HEAD, KEY, STAMP = range(9)
ANSI = {
    NORMAL: "", PROMPT: "\x1b[1m", SAY: "\x1b[33m", NOTE: "\x1b[2m", OK: "\x1b[32m",
    BAD: "\x1b[1;31m", HEAD: "\x1b[1;36m", KEY: "\x1b[7m", STAMP: "\x1b[2;37m",
}
RESET = "\x1b[0m"
SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"


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


# ---- the two columns ---------------------------------------------------------
class Pane:
    def __init__(self, ui, side, title, narrations):
        self.ui, self.side, self.title = ui, side, title
        self.narrations = narrations
        self.status = ""  # one live line under the column while its side works
        self.status_since = None
        self.started = time.monotonic()
        self.done = None

    def put(self, text="", style=NORMAL):
        self.ui.emit(self, str(text), style)

    def say(self, text):
        self.put(f"# {text}", SAY)

    def note(self, text):
        self.put(text, NOTE)

    def ok(self, text):
        self.put(text, OK)

    def bad(self, text):
        self.put(text, BAD)

    def beat(self, no, title):
        """A beat header in this column; the other column has its own clock."""
        self.put("", NORMAL)
        self.put(f"━━ beat {no} · {title}", HEAD)
        self.put(f"# {self.narrations[no]}", SAY)
        self.ui.transcript(f"== {self.side} beat {no}: {title}")

    def set_status(self, text, timed=False, keep_clock=False):
        """The live line; `timed` starts a running clock, `keep_clock` keeps it."""
        with self.ui.lock:
            self.status = text
            if not keep_clock:
                self.status_since = time.monotonic() if timed else None
        if text:
            self.ui.transcript(f"{self.side}~ {text}")
        self.ui.redraw_live()

    def status_text(self, frame):
        if not self.status:
            return ""
        clock = f"  {time.monotonic() - self.status_since:4.1f} s" if self.status_since is not None else ""
        return f"{SPINNER[frame % len(SPINNER)]} {self.status.strip()}{clock}"

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
            timeout=STEP_TIMEOUT,
        )
        out = proc.stdout.replace(str(root) + "/", "")
        if not quiet:
            for line in out.rstrip("\n").split("\n"):
                if line.strip():
                    self.put(line)
        return proc.returncode, out


# ---- the environment: paths, node, git ---------------------------------------
class Env:
    def __init__(self, bin_dir, port, agents, ci_seconds=3.0):
        self.bin = Path(bin_dir)
        self.port = port
        self.ci_seconds = ci_seconds
        self.overlap = 2
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
        try:
            p = subprocess.run(
                ["git", *args], cwd=cwd, env=self.git_env, stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT, text=True, check=False, timeout=STEP_TIMEOUT,
            )
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"git {' '.join(args)} in {cwd}: no answer in {STEP_TIMEOUT} s") from None
        if check and p.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} in {cwd}: {p.stdout}")
        return p

    def choir(self, *args):
        return subprocess.run(
            [str(self.bin / "choir"), *args], env=self.git_env, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, check=False, timeout=STEP_TIMEOUT,
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
                sys.exit(f"another take is running (pid {take.read_text()}); let it finish or Ctrl-C it")
            except (ValueError, ProcessLookupError):
                pass
            except PermissionError:
                sys.exit(f"another take is running (pid {take.read_text()}); let it finish or Ctrl-C it")
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
            "args": [], "timeout_seconds": max(60, int(self.ci_seconds * 20)),
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

    def queue_run(self):
        return self.post("/api/queue/run", {"repo": REPO, "branch": "main"})


# ---- the terminal ------------------------------------------------------------
class UI:
    """Two columns streamed into the terminal's own scrollback.

    Rows are printed as they happen, each in its column with a time stamp,
    the other column blank. The last line of the screen is the live row:
    both sides' status with a spinner and a clock, redrawn in place and
    cleared before every new row, so it never enters the scrollback.
    """

    GAP = " │ "
    STAMP_W = 7  # "  45.2 "

    def __init__(self, env, plain=False):
        self.env = env
        self.plain = plain or not sys.stdout.isatty() or os.environ.get("NO_COLOR") or os.environ.get("TERM") == "dumb"
        self.lock = threading.Lock()
        self.started = time.monotonic()
        self.width = shutil.get_terminal_size((160, 40)).columns
        self.live_shown = False
        self.frame = 0
        self.left = Pane(self, "L", "git: worktrees, branches, a careful maintainer", LEFT_NARRATION)
        self.right = Pane(self, "R", "choir: the same branches, one round", RIGHT_NARRATION)
        # Every row either column shows, with its time, so a take can be
        # read afterwards and sent to someone: .run/take.log.
        self.log = open(env.run / "take.log", "a")
        self.log_lock = threading.Lock()
        self.transcript(f"take: pid {os.getpid()}, {'plain' if self.plain else 'terminal'}, "
                        f"{self.width} columns, TERM {os.environ.get('TERM', '?')}")
        if hasattr(signal, "SIGWINCH"):
            signal.signal(signal.SIGWINCH, lambda *_: self._resized())

    def _resized(self):
        self.width = shutil.get_terminal_size((160, 40)).columns
        self.transcript(f"screen: resized to {self.width} columns")

    def transcript(self, line):
        with self.log_lock:
            self.log.write(f"{time.monotonic() - self.started:7.2f}  {line}\n")
            self.log.flush()

    # -- geometry --
    def cell_width(self):
        return max(30, (self.width - len(self.GAP) - 1) // 2)  # never the full width: no pending wrap under the live row

    def paint(self, text, style):
        if self.plain or not ANSI.get(style):
            return text
        return f"{ANSI[style]}{text}{RESET}"

    def row(self, left, right, lstyle=NORMAL, rstyle=NORMAL, stamp=""):
        """One terminal line: two cells padded to the column width."""
        cw = self.cell_width()
        body = cw - self.STAMP_W

        def cell(text, style, stamped):
            text = text[:body]
            pad = " " * (body - len(text))
            st = f"{stamp:>6} " if stamped and text else " " * self.STAMP_W
            return self.paint(st, STAMP) + self.paint(text, style) + pad

        return cell(left, lstyle, True) + self.paint(self.GAP, NOTE) + cell(right, rstyle, True)

    def wrapped(self, text):
        body = self.cell_width() - self.STAMP_W
        if not text.strip():
            return [""]
        lead = len(text) - len(text.lstrip(" "))
        return textwrap.wrap(text, body, subsequent_indent=" " * (lead + 4),
                             break_long_words=True, break_on_hyphens=False, drop_whitespace=False) or [""]

    # -- output --
    def emit(self, pane, text, style):
        stamp = f"{time.monotonic() - self.started:.1f}"
        self.transcript(f"{pane.side}│ {text}")
        lines = self.wrapped(text)
        with self.lock:
            self._clear_live()
            for i, line in enumerate(lines):
                st = stamp if i == 0 else ""
                if pane.side == "L":
                    out = self.row(line, "", style, NORMAL, st)
                else:
                    out = self.row("", line, NORMAL, style, st)
                sys.stdout.write(out.rstrip() + "\n")
            self._draw_live()
            sys.stdout.flush()

    def _clear_live(self):
        if self.live_shown:
            sys.stdout.write("\r\x1b[2K")
            self.live_shown = False

    def _draw_live(self):
        if self.plain:
            return
        l, r = self.left.status_text(self.frame), self.right.status_text(self.frame)
        if not l and not r:
            return
        sys.stdout.write("\r" + self.row(l, r, KEY, KEY).rstrip())
        self.live_shown = True

    def redraw_live(self):
        with self.lock:
            self._clear_live()
            self._draw_live()
            sys.stdout.flush()

    def tick(self):
        """The spinners and clocks, ten times a second, while anything is live."""
        while not getattr(self, "finished", False):
            time.sleep(0.1)
            self.frame += 1
            if self.left.status or self.right.status:
                self.redraw_live()

    def banner(self):
        cw = self.cell_width()
        rule = "─" * cw + "─┼─" + "─" * cw
        title = f" choir · the same {self.env.agents} agents, twice "
        sys.stdout.write(self.paint(title.center(len(rule), "═"), HEAD) + "\n")
        sys.stdout.write(self.row(self.left.title, self.right.title, HEAD, HEAD).rstrip() + "\n")
        sys.stdout.write(self.paint(rule, NOTE) + "\n")
        sys.stdout.flush()

    def closing(self):
        cw = self.cell_width()
        with self.lock:
            self._clear_live()
        sys.stdout.write(self.paint("─" * cw + "─┴─" + "─" * cw, NOTE) + "\n")
        for pane in (self.left, self.right):
            took = f"{pane.done - pane.started:.1f} s" if pane.done else "did not finish"
            self.emit(pane, f"  this side, start to finish: {took}", NOTE)
        self.left.note("  the honest summary: for disjoint work at a low conflict rate, branches and a merge queue are fine.")
        self.right.note("  the claim is what happens at the conflict, at the infra failure, and in what you can prove after.")
        sys.stdout.write(self.paint(f"transcript: {self.env.run / 'take.log'}   node log: {self.env.run / 'node.log'}", NOTE) + "\n")
        sys.stdout.flush()


# ---- helpers the beats share -------------------------------------------------
def ms(t0):
    return f"{(time.monotonic() - t0) * 1000:.0f} ms"


def free_port(first):
    """`first` if nothing listens on it, else the next loopback port that is free."""
    import socket

    for port in range(first, first + 50):
        with socket.socket() as s:
            # The node's listener sets SO_REUSEADDR, so a port left in
            # TIME_WAIT by the last take is free for it; probe the same way.
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                s.bind(("127.0.0.1", port))
            except OSError:
                continue
            return port
    sys.exit(f"no free port in {first}..{first + 49}; pass --port")


def seed_tree(path, agents, ci_seconds=0.0):
    """The repository both columns start from: a config, one file per
    service, and the tests. Three tests, each a line of shell, so the
    take can show a textual conflict (greeting), a real dependency
    (farewell) and a semantic conflict (ports) with nothing hidden."""
    (path / "config.toml").write_text(
        'greeting = "hello"\n\n[service]\nname = "app"\nport = 8080\n\n[features]\nfarewell = false\n'
    )
    test = path / "test.sh"
    test.write_text(
        "#!/bin/sh\n"
        f"# the repo's tests. They take {ci_seconds:g} s, the way a real suite takes minutes.\n"
        f"sleep {ci_seconds:g}\n"
        "fail=0\n"
        "# 1. the greeting is one quoted word, and the file carries no conflict marker\n"
        "grep -q '^<<<<<<<' config.toml && { echo 'conflict: config.toml carries markers'; fail=1; }\n"
        "grep -q '^greeting = \"[a-z]*\"$' config.toml || { echo 'greeting: not a quoted word'; fail=1; }\n"
        "# 2. a service that says farewell needs the feature switched on\n"
        "if grep -q '^farewell = true' svc/*.toml; then\n"
        "  grep -q '^farewell = true' config.toml || { echo 'farewell: a service uses it, config has it off'; fail=1; }\n"
        "fi\n"
        "# 3. two enabled services cannot share a port\n"
        "ports=$(for f in svc/*.toml; do grep -q '^enabled = true' \"$f\" && sed -n 's/^port = //p' \"$f\"; done | sort)\n"
        "dup=$(printf '%s\\n' \"$ports\" | uniq -d)\n"
        "[ -z \"$dup\" ] || { echo \"port $dup: two enabled services\"; fail=1; }\n"
        "exit $fail\n"
    )
    test.chmod(0o755)
    (path / "svc").mkdir()
    for name in agents:
        (path / "svc" / f"{name}.toml").write_text(f"enabled = false\nport = {port_of(name)}\n")


def port_of(name):
    return 9000 + int(name.rsplit("-", 1)[-1])


GREETINGS = ["hola", "bonjour", "ciao", "hallo", "hej", "salut", "servus", "aloha", "namaste",
             "salve", "ahoj", "hei", "moi", "oi", "yo"]
# The agents that touch the greeting line, in the order --overlap takes
# them: 03 and 07 always (beat 3 turns on 07), then more. 05, 09, 12, 15
# and 18 are kept out: beats 3 and 5 and the port clash need them clean.
OVERLAP_ORDER = [3, 7, 11, 14, 17, 20, 2, 6, 10, 13, 16, 19, 1, 4, 8]
PORT_CLASH = ("agent-18", "agent-15")  # 18 takes 15's port: clean merge, red together


def overlap_agents(env):
    picks = [n for n in OVERLAP_ORDER if n <= env.agents][: env.overlap]
    return [f"agent-{n:02d}" for n in picks]


def agent_change(wt, name, env):
    """What agent <name> does in its worktree. Returns the files touched.

    Everyone enables its service. The overlap agents also rewrite THE
    greeting line, each to a different word; 07 also switches the
    farewell feature on, which is what agent-12 will need in beat 3.
    agent-18 takes agent-15's port: a change that merges cleanly and
    fails the tests only next to 15's."""
    overlap = overlap_agents(env)
    port = port_of(PORT_CLASH[1]) if name == PORT_CLASH[0] else port_of(name)
    (wt / "svc" / f"{name}.toml").write_text(f"enabled = true\nport = {port}\n")
    files = f"svc/{name}.toml" + (f" (port {port}, same as {PORT_CLASH[1]})" if name == PORT_CLASH[0] else "")
    if name in overlap:
        cfg = wt / "config.toml"
        text = cfg.read_text().replace('greeting = "hello"', f'greeting = "{GREETINGS[overlap.index(name)]}"', 1)
        if name == "agent-07":
            text = text.replace("farewell = false", "farewell = true", 1)
        cfg.write_text(text)
        files += ", config.toml"
    return files


def run_ci(runner, cwd, env):
    """The maintainer's CI step: a shell runs the runner, so a runner that
    cannot start is exit 126, the way any CI script would see it.
    Returns (exit code, the tests' one-line reason)."""
    p = subprocess.run(["/bin/sh", "-c", f'exec "{runner}"'], cwd=cwd, env=env.git_env,
                       capture_output=True, text=True, check=False, timeout=STEP_TIMEOUT)
    if p.returncode == 126:
        return 126, "exit 126: could not run ci/run-tests"
    return p.returncode, "; ".join(p.stdout.strip().splitlines())


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


def parallel(fns, on_error=None):
    """Run functions at once; every failure is reported, the first re-raised."""
    errors = []

    def guarded(f):
        try:
            f()
        except BaseException as e:  # re-raised in the caller, once
            errors.append(e)
            if on_error:
                on_error(e)

    threads = [threading.Thread(target=guarded, args=(f,), daemon=True) for f in fns]
    for t in threads:
        t.start()
    for t in threads:
        while t.is_alive():  # a join with a timeout stays interruptible
            t.join(0.2)
    if errors:
        raise errors[0]


# ---- narration, one voice per column ----------------------------------------
LEFT_NARRATION = {
    0: "a bare repo, fast-forward only. The same seed, tests and agents as the other column.",
    1: "each agent enables its service on its own branch. The overlap agents also rewrite the greeting line; 18 takes 15's port. Nobody rejects a push: isolation is free.",
    2: "a batch queue, the way bors does it: merge everything pending in order, test once, and on red bisect for the culprit. Watch 07 and 18.",
    3: "agent-12's change needs the farewell feature, which only agent-07's branch switches on.",
    4: "what this side can prove afterwards: a commit graph, which is honest and good.",
    5: "two more branches: 05 breaks the greeting test, 09 adds a note; 18 is back with a free port. Then the CI runner loses its exec bit. Who gets blamed, and what happens once it is back.",
}
RIGHT_NARRATION = {
    0: "a choir node. The same seed, tests and agents as the other column.",
    1: "each agent enables its service on its own branch, proposed as refs/for/main/<agent>/enable. The same overlap, the same port clash. Nobody rejects a push here either.",
    2: "one queue round: a speculative train, every candidate tested on top of the ones before it, all at once. Watch 07 and 18.",
    3: "07 lands the conflict on main as-is, the node reads it three-sided, 12 builds on top of it, 07 resolves later.",
    4: "what this side can prove afterwards: every landing, check verdict and ref move as one signed hash chain, verified offline with the node's public key.",
    5: "the same three proposals and the same broken runner. Errored is a statement about the runner; Failed is a statement about the change.",
}


# ---- the left column: git alone ----------------------------------------------
def left_script(ui):
    env, L = ui.env, ui.left
    runner = env.left / "ci" / "run-tests"
    m = env.left / "maintainer"

    # beat 0
    L.beat(0, "one remote, one base")
    L.cmd(["git", "init", "-q", "--bare", "origin.git"], cwd=env.left, label="left ")
    hook = env.left / "origin.git" / "hooks" / "update"
    hook.write_text("#!/bin/sh\n# main is protected: fast-forward only, the way a branch rule does it\n"
                    "[ \"$1\" = refs/heads/main ] || exit 0\n"
                    "git merge-base --is-ancestor \"$2\" \"$3\" || { echo 'main: not a fast-forward' >&2; exit 1; }\n")
    hook.chmod(0o755)
    L.put("left $ install origin.git/hooks/update   # main: fast-forward only, like a branch rule", PROMPT)
    L.note("  every push is a compare-and-swap on its ref; nobody can overwrite anybody's branch, and main only moves forward")
    seed_side(ui, L, env.left, str(env.left / "origin.git"))

    # beat 1
    L.beat(1, f"{env.agents} agents, {env.agents} worktrees, {env.agents} branches")
    L.put("each $ git worktree add wt/<agent> -b <agent> main && edit && git commit && git push origin <agent>", PROMPT)
    push_all(ui, L, env.left, lambda n: n)

    # beat 2
    L.beat(2, f"integration: {env.agents} branches, one batch")
    env.git(env.left, "clone", "-q", str(env.left / "origin.git"), "maintainer")
    L.put("maintainer $ merge all pending branches in order; ci/run-tests once; on red, bisect; push main", PROMPT)
    t0 = time.monotonic()
    res = batch_land(ui, L, m, runner, [(n, f"origin/{n}") for n in env.names()])
    L.put(f"  {len(res['merged'])} merged, {len(res['blocked'])} blocked, {len(res['evicted'])} evicted, "
          f"{res['tests']} test runs one after another, {ms(t0)}", NOTE)
    if res["tests"] > 1:
        L.note(f"  the cost: {res['tests']} x {env.ci_seconds:g} s of tests in series: one for the batch, "
               f"{res['tests'] - 2} to find the culprit, one to land the rest")
    else:
        L.note(f"  the cost: one test run of {env.ci_seconds:g} s for the whole batch, plus a merge each")
    L.note("  agent-07's work is on a branch nobody else can see on main until its author comes back")
    fix_port(ui, L, env.left, lambda n: n)
    storm_left(ui, L, m, runner)

    # beat 3
    L.beat(3, "the conflict: a branch that waits")
    wt = env.left / "wt" / "agent-12"
    env.git(wt, "fetch", "-q", "origin")
    env.git(wt, "merge", "-q", "origin/main")
    (wt / "svc" / "agent-12.toml").write_text(f"enabled = true\nport = {port_of('agent-12')}\nfarewell = true\n")
    env.git(wt, "commit", "-q", "-am", "agent-12: farewell")
    L.put("agent-12 $ echo 'farewell = true' >> svc/agent-12.toml && git commit -qam farewell && ./test.sh", PROMPT)
    rc, why = run_ci(runner, wt, env)
    L.bad(f"  tests ✗  {why}")
    L.note("  the feature is on in exactly one place: agent-07's branch, the one the batch skipped")
    rc, _ = L.cmd(["git", "merge", "origin/agent-07"], cwd=wt, quiet=True)
    L.bad("  CONFLICT (content): Merge conflict in config.toml" if rc else "  merged")
    L.cmd(["git", "merge", "--abort"], cwd=wt)
    L.note("  agent-12 waits for agent-07, or resolves somebody else's conflict in its own worktree")
    L.note("  main does not know a conflict exists; a forge would show 07's PR as 'has conflicts'")

    # beat 4
    L.beat(4, "what this side can prove")
    L.cmd(["git", "log", "--oneline", "-3", "main"], cwd=m)
    L.note("  proof of order: parent pointers. signatures on merges: none configured. CI verdicts: not recorded.")
    L.note("  a forge adds a database of check runs and an audit log; you trust the forge for both")

    # beat 5
    L.beat(5, "a broken test runner is not a failing test")
    second_branches(ui, env.left, lambda n: f"HEAD:{n}-2")
    pending = [("agent-05", "origin/agent-05-2"), ("agent-09", "origin/agent-09-2")]
    if env.overlap == 2:  # otherwise 18, port fixed, went in with the rebased authors
        pending.append(("agent-18", "origin/agent-18"))

    def batch(broken):
        L.put("maintainer $ merge pending in order; ci/run-tests; on red, bisect", PROMPT)
        res = batch_land(ui, L, m, runner, pending)
        if broken:
            L.note(f"  exit 126 is 'could not run', but red is red to a queue: {len(res['evicted'])} authors paged "
                   f"for a runner they do not own, {res['tests']} test runs spent on it")

    L.put(f"$ chmod -x {typed([runner], env.run)}", PROMPT)
    runner.chmod(0o644)
    batch(True)
    L.put(f"$ chmod +x {typed([runner], env.run)}   # the runner is back", PROMPT)
    runner.chmod(0o755)
    batch(False)
    L.done = time.monotonic()


def batch_land(ui, L, m, runner, items):
    """A batch queue on plain git. `items` are (label, ref) in queue order.

    Merge every item onto main in order, skipping the ones that
    conflict; test the batch once; on red, bisect for the first item
    whose prefix fails, evict it, and test the rest again. Pushes main
    when green. This is bors; GitHub's queue is a train like choir's."""
    env = ui.env
    env.git(m, "fetch", "-q", "origin")
    env.git(m, "checkout", "-q", "main")
    env.git(m, "reset", "-q", "--hard", "origin/main")
    base = env.git(m, "rev-parse", "HEAD").stdout.strip()
    blocked, evicted, tests = [], [], 0

    def build(batch):
        env.git(m, "reset", "-q", "--hard", base)
        for label, ref in batch:
            p = env.git(m, "merge", "-q", "--no-ff", "-m", f"merge {label}", ref, check=False)
            if p.returncode != 0:
                env.git(m, "merge", "--abort")
                raise RuntimeError(f"{label} conflicted inside a batch that was clean before")

    # first pass: who merges at all
    cands = []
    env.git(m, "reset", "-q", "--hard", base)
    for label, ref in items:
        p = env.git(m, "merge", "-q", "--no-ff", "-m", f"merge {label}", ref, check=False)
        if p.returncode != 0:
            conflict = [l.split(" in ", 1)[-1] for l in p.stdout.splitlines() if l.startswith("CONFLICT")]
            env.git(m, "merge", "--abort")
            blocked.append(label)
            L.bad(f"  skip {label}  CONFLICT {', '.join(conflict)}  → author paged, branch waits")
        else:
            cands.append((label, ref))
    L.put(f"  batch: {compact([l for l, _ in cands]) or '-'}  ({len(cands)})")

    def test(batch, what):
        nonlocal tests
        tests += 1
        L.set_status(f"test run {tests}: {what}", timed=True)
        rc, why = run_ci(runner, m, env)
        L.set_status("")
        return rc, why

    while cands:
        rc, why = test(cands, f"the batch of {len(cands)}")
        if rc == 0:
            L.put(f"  test run {tests} on {len(cands)}  ✓  → main", OK)
            break
        L.bad(f"  test run {tests} on {len(cands)}  ✗  {why}")
        # bisect: the smallest prefix that fails names the culprit
        lo, hi = 1, len(cands)
        while lo < hi:
            mid = (lo + hi) // 2
            build(cands[:mid])
            rc, why = test(cands[:mid], f"bisecting, prefix of {mid}")
            L.put(f"  test run {tests} on {mid}  {'✓' if rc == 0 else '✗'}", OK if rc == 0 else BAD)
            if rc == 0:
                lo = mid + 1
            else:
                hi = mid
        culprit = cands[lo - 1][0]
        evicted.append(culprit)
        L.bad(f"  evict {culprit}  tests ✗  → author paged")
        cands = cands[: lo - 1] + cands[lo:]
        build(cands)
    if cands:
        env.git(m, "push", "-q", "origin", "main")
    else:
        env.git(m, "reset", "-q", "--hard", base)
    return {"merged": [l for l, _ in cands], "blocked": blocked, "evicted": evicted, "tests": tests}


def storm_left(ui, L, m, runner):
    """--overlap beyond 2: the other greeting authors rebase onto main,
    keep main's greeting, push again, and the next batch takes them."""
    env = ui.env
    extras = [n for n in overlap_agents(env) if n not in ("agent-03", "agent-07")]
    if not extras:
        return
    L.say(f"{len(extras)} more authors rewrote the greeting line and were skipped, paged, and now rebase: each keeps main's word and pushes again.")
    t0 = time.monotonic()
    for name in extras:
        rebase_by_hand(ui, L, env.left / "wt" / name, name, lambda n: n)
    L.put("maintainer $ merge pending in order; ci/run-tests; on red, bisect", PROMPT)
    res = batch_land(ui, L, m, runner, [(n, f"origin/{n}") for n in extras + [PORT_CLASH[0]]])
    L.note(f"  {len(extras)} rebases by {len(extras)} paged authors, then one batch, {res['tests']} test run(s), {ms(t0)}")


def fix_port(ui, pane, side, refspec):
    """agent-18 picks a free port and pushes again; the next batch or
    round takes it. Same on both sides."""
    env = ui.env
    name = PORT_CLASH[0]
    wt = side / "wt" / name
    (wt / "svc" / f"{name}.toml").write_text(f"enabled = true\nport = {port_of(name)}\n")
    env.git(wt, "commit", "-q", "-am", f"{name}: a port of its own")
    env.git(wt, "push", "-q", "-f", "origin", refspec(name))
    pane.put(f"{name} $ sed -i 's/port = {port_of(PORT_CLASH[1])}/port = {port_of(name)}/' svc/{name}.toml && git commit -qam 'a port of its own' && git push -f origin {refspec(name)}", PROMPT)


def rebase_by_hand(ui, pane, wt, name, refspec):
    env = ui.env
    env.git(wt, "fetch", "-q", "origin")
    env.git(wt, "reset", "-q", "--hard", "origin/main")
    agent_change(wt, name, env)
    env.git(wt, "commit", "-q", "-am", f"{name}: enable (rebased on main, greeting conflict resolved by its author)")
    env.git(wt, "push", "-q", "-f", "origin", refspec(name))
    pane.put(f"{name} $ git rebase origin/main   # keeps main's greeting, pushes again", PROMPT)


# ---- the right column: choir -------------------------------------------------
def right_script(ui):
    env, R = ui.env, ui.right
    runner = env.run / "ci" / "run-tests"

    # beat 0
    R.beat(0, "one node, one base")
    R.note(f"  choir-node on {env.api}, one repo {REPO}, the queue runs the repo's test.sh")
    R.cmd(["curl", "-s", "-o", "/dev/null", "-w", "healthz %{http_code}\\n", f"{env.api}/healthz"], label="right ")
    seed_side(ui, R, env.right, env.url)

    # beat 1
    R.beat(1, f"{env.agents} agents, {env.agents} worktrees, {env.agents} proposals")
    R.put("each $ git worktree add wt/<agent> -b <agent> main && edit && git commit && git push origin HEAD:refs/for/main/<agent>/enable", PROMPT)
    push_all(ui, R, env.right, lambda n: f"HEAD:refs/for/main/{n}/enable")

    # beat 2
    R.beat(2, f"integration: {env.agents} proposals, one round")
    r = one_round(ui, R, f"one round: {env.agents} speculative merges, every candidate tested at once")
    R.note(f"  the cost: {env.ci_seconds:g} s of tests, {r['ci_runs']} runs side by side, plus one merge per candidate")
    R.note("  every eviction is a verdict inside the round, with its reason, not a stop: the rest landed behind them in the same call")
    fix_port(ui, R, env.right, lambda n: f"HEAD:refs/for/main/{n}/enable")
    storm_right(ui, R)

    # beat 3
    R.beat(3, "the conflict: a value on main")
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
    R.say("agent-12 pulls main: the conflict commit, farewell switched on. Builds on it, tests, lands.")
    env.git(wt12, "pull", "--no-rebase", "-q", "origin", "main")
    (wt12 / "svc" / "agent-12.toml").write_text(f"enabled = true\nport = {port_of('agent-12')}\nfarewell = true\n")
    R.put("agent-12 $ echo 'farewell = true' >> svc/agent-12.toml && ./test.sh", PROMPT)
    rc, why = run_ci(runner, wt12, env)
    R.put(f"  tests {'✓' if rc == 0 else '✗'}  {why}".rstrip(), OK if rc == 0 else BAD)
    R.note("  the farewell check passes: 07's feature switch is on main. The one red is 07's open conflict, and it is 07's to close, not 12's")
    R.note("  12's work goes to main now; it does not wait on somebody else's line")
    R.cmd(["git", "commit", "-q", "-am", "agent-12: farewell (on top of the unresolved conflict)"], cwd=wt12)
    rc, out = R.cmd(["git", "push", "origin", "HEAD:main"], cwd=wt12, quiet=True)
    for line in out.splitlines():
        if "->" in line or "rejected" in line:
            R.put("  " + line.strip(), OK if rc == 0 else BAD)
    R.say("agent-07 comes back and resolves. One more commit; the history keeps the conflict.")
    env.git(wt07, "pull", "--no-rebase", "-q", "origin", "main")
    cfg = wt07 / "config.toml"
    cfg.write_text(cfg.read_text().split("<<<<<<<")[0] + 'greeting = "hola"\n' + cfg.read_text().split(">>>>>>>", 1)[1].split("\n", 1)[1])
    R.cmd(["git", "commit", "-q", "-am", "resolve: greeting is Spanish"], cwd=wt07)
    env.git(wt07, "push", "-q", "origin", "HEAD:main")
    R.put("agent-07 $ ./test.sh", PROMPT)
    rc, why = run_ci(runner, wt07, env)
    R.put(f"  tests {'✓' if rc == 0 else '✗'}  {why}".rstrip(), OK if rc == 0 else BAD)
    R.cmd(["git", "log", "--oneline", "-4", "origin/main"], cwd=wt07)

    # beat 4
    R.beat(4, "what this side can prove")
    entries = env.get("/api/log?from=0")["entries"]
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
    R.note("  one key, the node's. This proves the order and that nothing was rewritten or dropped, and anyone can check it offline.")
    R.note("  it does not prove who pushed: git pushes reach the log as the node's own signed ops. A forge's audit log you trust; this one you verify.")

    # beat 5
    R.beat(5, "a broken test runner is not a failing test")
    second_branches(ui, env.right, lambda n: f"HEAD:refs/for/main/{n}/second")

    def round_two(broken):
        r = one_round(ui, R, "one round over the three open proposals")
        if broken:
            R.note("  Errored is a statement about us: all requeued, nothing evicted, main unmoved, reason recorded")
        else:
            R.note(f"  Failed is a statement about the change: only agent-05 pays; the others landed in the same round, {r['ci_runs']} test runs")

    R.put(f"$ chmod -x {typed([runner], env.run)}", PROMPT)
    runner.chmod(0o644)
    round_two(True)
    R.put(f"$ chmod +x {typed([runner], env.run)}   # the runner is back", PROMPT)
    runner.chmod(0o755)
    round_two(False)
    R.done = time.monotonic()


def one_round(ui, R, status):
    env = ui.env
    t0 = time.monotonic()
    R.put("$ curl -X POST /api/queue/run  {repo, branch: main}", PROMPT)
    R.set_status(status, timed=True)
    names = round_names(env)
    r = env.queue_run()
    R.set_status("")
    for line in round_lines(env, r, names):
        R.put(line, BAD if ("Conflict" in line or "CiFailure" in line or "could not" in line) else NORMAL)
    R.put(f"  {len(r['merged'])} landed, {len(r['rejected'])} evicted, {r['ci_runs']} CI runs in one train, {ms(t0)}", NOTE)
    r["names"] = {names.get(i, "").split("/")[0] for i in r["merged"]}
    return r


def storm_right(ui, R):
    """--overlap beyond 2: the other greeting authors rebase onto main,
    keep main's greeting, propose again, and the next round takes them."""
    env = ui.env
    extras = [n for n in overlap_agents(env) if n not in ("agent-03", "agent-07")]
    if not extras:
        return
    R.say(f"{len(extras)} more authors rewrote the greeting line and were evicted, each with the reason in the round's answer; now they rebase: each keeps main's word and proposes again.")
    t0 = time.monotonic()
    rounds = runs = 0
    waiting = list(extras)
    while waiting:
        for name in waiting:
            rebase_by_hand(ui, R, env.right / "wt" / name, name, lambda n: f"HEAD:refs/for/main/{n}/enable")
        r = one_round(ui, R, f"one round over {len(waiting)} rebased proposals")
        rounds += 1
        runs += r["ci_runs"]
        waiting = [n for n in waiting if n not in r["names"]]
    R.note(f"  {len(extras)} rebases, {rounds} round(s), {runs} test runs, {ms(t0)}. The same work as the other column; what differed was the first round: one call, a verdict each")


# ---- steps both columns share, each on its own tree --------------------------
def seed_side(ui, pane, side, url):
    env = ui.env
    seed_dir = side / "seed"
    env.git(side, "clone", "-q", url, "seed")
    seed_tree(seed_dir, env.names(), env.ci_seconds)
    env.git(seed_dir, "add", ".")
    env.git(seed_dir, "commit", "-q", "-m", "base: config, tests, the services")
    env.git(seed_dir, "push", "-q", "origin", "HEAD:main")
    pane.put("seed $ git push origin HEAD:main", PROMPT)
    pane.note(f"  base: config.toml, test.sh (three checks, {env.ci_seconds:g} s per run), svc/agent-01..{env.agents:02d}.toml")


def push_all(ui, pane, side, refspec):
    """Every agent: a worktree, a branch, an edit, a commit, a push; all at once."""
    env = ui.env
    seed = side / "seed"
    t0 = time.monotonic()
    counters = [0, 0]  # pushed, rejected
    # `git worktree add` scans .git/worktrees/* while a sibling is still
    # writing its own entry, so creation is serialised per repo; the
    # edits, commits and pushes below stay concurrent.
    adding = threading.Lock()

    def one(name):
        wt = side / "wt" / name
        with adding:
            env.git(seed, "worktree", "add", "-q", str(wt), "-b", name, "main")
        files = agent_change(wt, name, env)
        env.git(wt, "add", ".")
        env.git(wt, "commit", "-q", "-m", f"{name}: enable")
        p = env.git(wt, "push", "-q", "origin", refspec(name), check=False)
        with ui.lock:
            counters[0 if p.returncode == 0 else 1] += 1
        pane.put(f"  {name}  {'pushed' if p.returncode == 0 else 'REJECTED'}  ({files})", OK if p.returncode == 0 else BAD)
        pane.set_status(f"pushed {counters[0]}  rejected {counters[1]}", keep_clock=True)

    pane.set_status(f"{env.agents} agents at work", timed=True)
    parallel([lambda n=n: one(n) for n in env.names()])
    pane.set_status("")
    pane.put(f"  {counters[0]} pushed, {counters[1]} rejected, {ms(t0)}", NOTE)


def second_branches(ui, side, refspec):
    """Beat 5's two branches: 05 breaks the greeting test, 09 adds a note."""
    env = ui.env
    env.git(side / "seed", "fetch", "-q", "origin")
    for name, edit in (("agent-05", lambda wt: (wt / "config.toml").write_text(
                            re.sub(r"^greeting = .*$", "greeting = hi", (wt / "config.toml").read_text(), count=1, flags=re.M))),
                       ("agent-09", lambda wt: (wt / "NOTES.md").write_text("Run ./test.sh before you push.\n"))):
        wt = side / "wt" / name
        env.git(wt, "checkout", "-q", "--detach", "origin/main")
        env.git(wt, "checkout", "-q", "-b", f"{name}-2")
        edit(wt)
        env.git(wt, "add", ".")
        env.git(wt, "commit", "-q", "-m", f"{name}: {'unquoted greeting (breaks test 1)' if name.endswith('05') else 'notes'}")
        env.git(wt, "push", "-q", "origin", refspec(name))


# ---- main --------------------------------------------------------------------
def play(ui):
    ui.banner()
    ticker = threading.Thread(target=ui.tick, daemon=True)
    ticker.start()

    def report(e):
        side = ui.left if threading.current_thread().name == "left" else ui.right
        side.set_status("")
        side.bad(f"demo error: {e!r}")

    try:
        parallel([named("left", left_script, ui), named("right", right_script, ui)], on_error=report)
    finally:
        ui.finished = True
        ui.closing()


def named(name, script, ui):
    def run():
        threading.current_thread().name = name
        script(ui)
    return run


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, help="directory holding choir-node and choir")
    ap.add_argument("--plain", "--dump", action="store_true", help="no colour, no live rows")
    ap.add_argument("--port", type=int, default=None, help="default 8447, or the next free port")
    ap.add_argument("--agents", type=int, default=20, help="default 20, at least 18")
    ap.add_argument("--overlap", type=int, default=2, help="agents that rewrite the greeting line (default 2, at least 2)")
    ap.add_argument("--ci-seconds", type=float, default=3.0, help="how long test.sh takes (default 3)")
    args = ap.parse_args()
    if args.agents < 18 or args.ci_seconds < 0:
        sys.exit("the beats name agents 05, 07, 09, 12, 15 and 18: --agents 18 or more; --ci-seconds 0 or more")
    most = len([n for n in OVERLAP_ORDER if n <= args.agents])
    if not 2 <= args.overlap <= most:
        sys.exit(f"--overlap takes 2 to {most} with {args.agents} agents")
    env = Env(args.bin, args.port or free_port(8447), args.agents, args.ci_seconds)
    env.overlap = args.overlap
    for b in ("choir-node", "choir"):
        if not (env.bin / b).exists():
            sys.exit(f"no {b} in {env.bin}; run demo/run.sh")
    for tool in ("git", "curl"):
        if shutil.which(tool) is None:
            sys.exit(f"{tool} is not on PATH")
    # A closed tab or a kill must still take the node down: turn the signal
    # into the SystemExit that the finally below handles.
    for sig in (signal.SIGHUP, signal.SIGTERM):
        signal.signal(sig, lambda *_: sys.exit(1))
    env.reset()
    env.start_node()
    ui = UI(env, plain=args.plain)
    try:
        play(ui)
    except KeyboardInterrupt:
        sys.stdout.write("\n")
        sys.exit(f"stopped; transcript so far: {env.run / 'take.log'}")
    except Exception as e:
        sys.exit(f"demo error: {e}\ntranscript: {env.run / 'take.log'}   node log: {env.run / 'node.log'}")
    finally:
        env.stop_node()


if __name__ == "__main__":
    main()
