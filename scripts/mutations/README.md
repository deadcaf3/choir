# Mutation sets

Each directory here is one set of mutations for `scripts/mutation_run.sh`:

```sh
sh scripts/mutation_run.sh scripts/mutations/d68 -p choir-node --test it node_queue::
```

A mutation is a `*.py` file that edits exactly one tracked file in place
and fails loudly if its anchor does not match exactly once. The runner
asks git what changed, so a mutation that matches nothing is reported as
proving nothing rather than as a survivor.

**Sets are tracked on purpose.** The `mut-bridge2` set was described only
in a session prompt, and when three of its seven mutations needed
re-running nobody could reproduce them: the anchors were gone, and one
had silently become ambiguous. A set nobody can re-run is a set that
stops being run.

Naming: `<decision>/<id>_<what_it_breaks>.py`, so a report line names the
property rather than a number.
