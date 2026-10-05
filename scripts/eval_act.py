"""Compare language models on Whirl's ACT, GOAL, ai: targets, EXTRACT, and JUDGE: run each model
against each eval task, then write a dated summary. See evals/act/README.md.

Each task is a .whirl flow. Runs are kept under evals/act/runs/ and never
changed; the summary reads them all, so a session can stop and resume.
"""
import argparse
import datetime
import functools
import http.server
import json
import math
import os
import re
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVALS = ROOT / "evals" / "act"
SETS = ("local", "live")
DEFAULT_N = {"local": 10, "live": 1}
# A task whose file name ends with this passes only when ACT finds no
# element (evals/act/README.md).
NO_MATCH_SUFFIX = ".no-match.whirl"
# A task whose file name ends with this passes only when an ai: target
# matches several elements and fails with strictness (SPEC 6.3).
AMBIGUOUS_SUFFIX = ".ambiguous.whirl"
# A task whose file name ends with this passes only when JUDGE answers no
# (SPEC 9.8).
JUDGE_FALSE_SUFFIX = ".judge-false.whirl"
# A task whose file name ends with this passes only when GOAL answers
# impossible (SPEC 7.7).
IMPOSSIBLE_SUFFIX = ".impossible.whirl"
# Each special suffix and the error code that makes its task pass.
EXPECTED_FAILURES = {
    NO_MATCH_SUFFIX: "act-no-match",
    AMBIGUOUS_SUFFIX: "strictness",
    JUDGE_FALSE_SUFFIX: "judge-false",
    IMPOSSIBLE_SUFFIX: "goal-impossible",
}
# A task whose file name ends with this passes only when the flow passes with
# the step warning: JUDGE answers unsure.
UNSURE_SUFFIX = ".unsure.whirl"
EXPECTED_WARNINGS = {UNSURE_SUFFIX: "judge-unsure"}
# The value the login task fills through {{env.EVAL_PASSWORD}}. It is not a
# secret; it only has to reach the page without reaching the model.
EVAL_PASSWORD = "eval-password-5d1c"
# A model name with this prefix runs with --jev: Jev plans first, and the
# rest of the name is the fallback model (SPEC 7.4).
JEV_PREFIX = "jev:"


@dataclass(frozen=True)
class Task:
    set: str
    name: str
    path: Path
    # The error code the task expects, when it passes only by failing.
    expected_failure: str | None


def expected_failure(file_name):
    """The error code a task file expects, from its suffix."""
    for suffix, code in EXPECTED_FAILURES.items():
        if file_name.endswith(suffix):
            return code
    return None


def expected_warning(file_name):
    """The step warning a task file expects, from its suffix."""
    for suffix, code in EXPECTED_WARNINGS.items():
        if file_name.endswith(suffix):
            return code
    return None


def load_tasks(set_name, only=()):
    """The tasks of one set, sorted by name, limited to `only` when given."""
    tasks = []
    for path in sorted((EVALS / set_name / "flows").glob("*.whirl")):
        name = task_name(path)
        if not only or name in only:
            tasks.append(Task(set_name, name, path, expected_failure(path.name)))
    return tasks


def load_models(requested):
    if requested:
        return list(requested)
    lines = (EVALS / "models.txt").read_text().splitlines()
    return [line.strip() for line in lines if line.strip() and not line.startswith("#")]


def whirl_model(model):
    """The model option and the extra flags for a model name."""
    if model.startswith(JEV_PREFIX):
        return model[len(JEV_PREFIX):], ["--jev"]
    return model, []


def slug(model):
    return re.sub(r"[^a-z0-9.]+", "-", model.lower()).strip("-")


@dataclass
class Result:
    """One task's outcome in one run."""

    task: str
    model: str
    # "pass", "fail", "drift" (live precheck failed), or "error" (harness).
    outcome: str
    code: str | None = None
    duration_ms: int | None = None
    model_calls: int | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cost_usd_micros: int | None = None
    priced: bool = False
    # Actions that ran, and how many of them Jev chose.
    actions: int = 0
    jev_actions: int = 0

    @property
    def good(self):
        """True when the run is evidence about the model."""
        return self.outcome in ("pass", "fail")


def task_name(file_path):
    name = Path(file_path).name
    for suffix in (*EXPECTED_FAILURES, *EXPECTED_WARNINGS, ".whirl"):
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


def classify(file_report, model):
    """Classifies one flow's result from a Whirl JSON report."""
    name = task_name(file_report["path"])
    expects = expected_failure(Path(file_report["path"]).name)
    steps = [step for entry in file_report["entries"] for step in entry["steps"]]
    # A task can have several ACT lines and ai: targets; its measurements
    # are their sums.
    act_steps = [step for step in steps if step.get("act") is not None]
    ai_steps = [step for step in steps if step.get("ai") is not None]
    ai_steps += [step for step in steps if step.get("extract") is not None]
    ai_steps += [step for step in steps if step.get("judge") is not None]
    ai_steps += [step for step in steps if step.get("goal") is not None]
    result = Result(name, model, "fail")
    if act_steps or ai_steps:
        usages = [step["act"]["usage"] for step in act_steps]
        usages += [
            (step.get("ai") or step.get("extract") or step.get("judge") or step["goal"])["usage"]
            for step in ai_steps
        ]
        act_steps = act_steps + ai_steps
        result.duration_ms = sum(step["durationMs"] for step in act_steps)
        result.model_calls = sum(usage["modelCalls"] for usage in usages)
        result.input_tokens = sum(usage["inputTokens"] for usage in usages)
        result.output_tokens = sum(usage["outputTokens"] for usage in usages)
        # A step with no model call cost nothing; older reports leave its
        # cost out.
        costs = [usage.get("costUsdMicros", 0 if usage["modelCalls"] == 0 else None) for usage in usages]
        result.priced = all(cost is not None for cost in costs)
        result.cost_usd_micros = sum(costs) if result.priced else None
        actions = [action for step in act_steps for action in (step.get("act") or {}).get("actions", [])]
        result.actions = len(actions)
        result.jev_actions = len([action for action in actions if action.get("plannedBy") == "jev"])

    failed = next((step for step in steps if step["status"] in ("failed", "error")), None)
    result.code = failed["error"]["code"] if failed and failed.get("error") else None

    entries = file_report["entries"]
    if file_report["status"] == "error":
        result.outcome = "error"
    elif entries and entries[0]["name"].startswith("precheck") and entries[0]["status"] != "passed":
        result.outcome = "drift"
    elif expects:
        result.outcome = "pass" if result.code == expects else "fail"
    elif warns := expected_warning(Path(file_report["path"]).name):
        warned = any(warning["code"] == warns for step in steps for warning in step.get("warnings", []))
        result.outcome = "pass" if file_report["status"] == "passed" and warned else "fail"
    else:
        result.outcome = "pass" if file_report["status"] == "passed" else "fail"
    return result


def load_results(set_name, models):
    """Every task result on disk for the set's selected models."""
    results = []
    for model in models:
        model_dir = EVALS / "runs" / set_name / slug(model)
        for report_path in sorted(model_dir.glob("*/report.json")):
            report = json.loads(report_path.read_text())
            for file_report in report["files"]:
                results.append(classify(file_report, model))
    return results


def good_counts(results):
    counts = {}
    for result in results:
        if result.good:
            key = (result.model, result.task)
            counts[key] = counts.get(key, 0) + 1
    return counts


def shortfalls(tasks, models, results, n):
    """The tasks each model still needs, and how many runs each."""
    counts = good_counts(results)
    plan = {}
    for model in models:
        missing = {task.name: n - counts.get((model, task.name), 0) for task in tasks}
        missing = {name: count for name, count in missing.items() if count > 0}
        if missing:
            plan[model] = missing
    return plan


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    """Serves the site. A `delay=MS` query parameter holds the response
    that long, like a slow API on a real site."""

    def do_GET(self):
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
        delay = query.get("delay", ["0"])[0]
        if delay.isdigit():
            time.sleep(min(int(delay), 10_000) / 1000)
        super().do_GET()

    def log_message(self, *args):
        pass


class SiteServer:
    """Serves the local set's pages on 127.0.0.1. Pages reach a second
    site through localhost, the same server under another host name."""

    def __enter__(self):
        handler = functools.partial(QuietHandler, directory=str(EVALS / "local" / "site"))
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()


def whirl_binary():
    return os.environ.get("WHIRL_BIN", str(ROOT / "target" / "debug" / "whirl"))


def run_pass(set_name, model, tasks, env):
    """Runs one Whirl invocation for one model over the given tasks."""
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    run_dir = EVALS / "runs" / set_name / slug(model) / stamp
    run_dir.mkdir(parents=True)
    (run_dir / "meta.json").write_text(json.dumps({"model": model, "set": set_name}, indent=2) + "\n")
    option, flags = whirl_model(model)
    command = [
        whirl_binary(),
        *flags,
        "--var", f"model={option}",
        "--report-json", str(run_dir / "report.json"),
        "--artifacts", str(run_dir / "artifacts"),
        *[str(task.path) for task in tasks],
    ]
    with (run_dir / "stdout.txt").open("w") as stdout:
        code = subprocess.run(command, cwd=ROOT, env=env, stdout=stdout, stderr=subprocess.STDOUT).returncode
    # 0, 1, and 3 are run outcomes; 2 and 4 mean the flows or the command
    # are wrong, and repeating would not help.
    if code in (2, 4) or not (run_dir / "report.json").exists():
        sys.exit(f"whirl exited {code}; see {run_dir / 'stdout.txt'}")


def run_set(set_name, tasks, models, n, env):
    """Runs passes until every task has n good runs for every model, or a
    pass adds no good run."""
    while True:
        results = load_results(set_name, models)
        plan = shortfalls(tasks, models, results, n)
        if not plan:
            return
        before = len([result for result in results if result.good])
        for model, missing in plan.items():
            pass_tasks = [task for task in tasks if task.name in missing]
            print(f"{set_name}: {model}: {len(pass_tasks)} task(s)", flush=True)
            run_pass(set_name, model, pass_tasks, env)
        after = len([result for result in load_results(set_name, models) if result.good])
        if after == before:
            print(f"{set_name}: a pass added no good run; stopping", flush=True)
            return


def percentile(values, fraction):
    ordered = sorted(values)
    index = max(0, math.ceil(fraction * len(ordered)) - 1)
    return ordered[index]


def seconds(ms):
    return f"{ms / 1000:.1f}s"


def tokens(value):
    return f"{value / 1000:.1f}k" if value >= 1000 else f"{value:.0f}"


def dollars(micros):
    return f"${micros / 1_000_000:.4f}"


@dataclass
class ModelSummary:
    model: str
    results: list = field(default_factory=list)

    def of(self, outcome):
        return [result for result in self.results if result.outcome == outcome]

    def row(self):
        good = [result for result in self.results if result.good]
        passes = len(self.of("pass"))
        cells = [f"`{self.model}`"]
        if good:
            rate = passes / len(good)
            error = math.sqrt(rate * (1 - rate) / len(good))
            cells.append(f"{rate:.2f} ± {error:.2f}")
        else:
            cells.append("n/a")
        cells += [str(len(good)), str(len(self.of("drift"))), str(len(self.of("error")))]
        measured = [result for result in good if result.duration_ms is not None]
        if measured:
            durations = [result.duration_ms for result in measured]
            cells += [seconds(statistics.median(durations)), seconds(percentile(durations, 0.9))]
            cells.append(f"{statistics.mean(result.model_calls for result in measured):.2f}")
            actions = sum(result.actions for result in measured)
            if self.model.startswith(JEV_PREFIX) and actions:
                cells.append(f"{sum(result.jev_actions for result in measured) / actions:.2f}")
            else:
                cells.append("-")
            cells.append(tokens(statistics.mean(result.input_tokens for result in measured)))
            cells.append(tokens(statistics.mean(result.output_tokens for result in measured)))
            if all(result.priced for result in measured):
                costs = [result.cost_usd_micros for result in measured]
                cells += [dollars(statistics.mean(costs)), dollars(sum(costs))]
            else:
                cells += ["n/a", "n/a"]
        else:
            cells += ["n/a"] * 8
        return "| " + " | ".join(cells) + " |"


def git_commit():
    try:
        return subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def whirl_version():
    try:
        return subprocess.run([whirl_binary(), "--version"], capture_output=True, text=True).stdout.strip()
    except OSError:
        return "unknown"


def render_summary(set_name, tasks, models, results, n, date, version, commit):
    """The markdown summary of one set."""
    by_model = {model: ModelSummary(model) for model in models}
    for result in results:
        by_model[result.model].results.append(result)
    lines = [
        f"# ACT evals: {set_name} set, {date}",
        "",
        f"- Whirl: {version}, commit `{commit}`",
        f"- Target runs per task (`-n`): {n}",
        f"- Tasks: {len(tasks)}",
        "",
        "Pass rate counts only good runs, with its standard error. Drift (a live",
        "precheck failed) and errors (exit 3: network, credentials, shim) are",
        "left out. Times are the task's ACT steps: snapshots, model calls, and actions.",
        "Costs use catalog prices and show n/a when any run was unpriced.",
        f"A `{JEV_PREFIX}` model runs with `--jev`: Jev plans first, and the named model",
        "plans when Jev is unsure. \"by Jev\" is the share of actions Jev chose. Calls",
        "and tokens count the model only; costs include Jev's requests.",
        "",
        "## Leaderboard",
        "",
        "| model | pass | good runs | drift | errors | p50 ACT | p90 ACT | calls | by Jev | in tok | out tok | $/task | $ total |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    lines += [by_model[model].row() for model in models]
    lines += ["", "## Per task", "", "Passes over good runs.", ""]
    lines.append("| task | " + " | ".join(f"`{model}`" for model in models) + " |")
    lines.append("| --- |" + " --- |" * len(models))
    for task in tasks:
        cells = []
        for model in models:
            good = [r for r in by_model[model].results if r.task == task.name and r.good]
            passes = len([r for r in good if r.outcome == "pass"])
            cells.append(f"{passes}/{len(good)}" if good else "-")
        lines.append(f"| {task.name} | " + " | ".join(cells) + " |")
    lines += ["", "## Failures", "", "Error codes of failed runs, per model.", ""]
    any_failure = False
    for model in models:
        codes = {}
        for result in by_model[model].of("fail"):
            codes[result.code or "passed"] = codes.get(result.code or "passed", 0) + 1
        if codes:
            any_failure = True
            listed = ", ".join(f"`{code}` {count}" for code, count in sorted(codes.items()))
            lines.append(f"- `{model}`: {listed}")
    if not any_failure:
        lines.append("None.")
    if set_name == "live":
        lines += ["", "## Drift", "", f"Tasks whose precheck failed, as of {date}.", ""]
        drifted = sorted({(r.task, r.model) for m in models for r in by_model[m].of("drift")})
        lines += [f"- {task} (`{model}`)" for task, model in drifted] or ["None."]
    return "\n".join(lines) + "\n"


def write_summary(set_name, tasks, models, n):
    results = [r for r in load_results(set_name, models) if r.task in {t.name for t in tasks}]
    date = datetime.date.today().isoformat()
    text = render_summary(set_name, tasks, models, results, n, date, whirl_version(), git_commit())
    path = EVALS / "results" / f"{date}-{set_name}.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    print(f"wrote {path.relative_to(ROOT)}")


def print_preview(set_name, tasks, models, n):
    plan = shortfalls(tasks, models, load_results(set_name, models), n)
    total = sum(sum(missing.values()) for missing in plan.values())
    print(f"{set_name}: {len(tasks)} task(s) × {len(models)} model(s), -n {n}: {total} run(s) to go")
    for model in models:
        missing = plan.get(model, {})
        print(f"  {model}: {sum(missing.values())} run(s) over {len(missing)} task(s)")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--set", choices=(*SETS, "all"), default="local")
    parser.add_argument("-m", "--model", action="append", default=[], help="repeatable; overrides models.txt")
    parser.add_argument("-t", "--task", action="append", default=[], help="repeatable; limits the tasks")
    parser.add_argument("-n", type=int, help="target good runs per model and task")
    parser.add_argument("--preview", action="store_true", help="print the planned runs and exit")
    parser.add_argument("--summarize", action="store_true", help="summarize existing runs without running")
    args = parser.parse_args(argv)

    models = load_models(args.model)
    env = {**os.environ, "EVAL_PASSWORD": EVAL_PASSWORD}
    for set_name in SETS if args.set == "all" else (args.set,):
        tasks = load_tasks(set_name, args.task)
        if not tasks:
            continue
        n = args.n if args.n is not None else DEFAULT_N[set_name]
        if args.preview:
            print_preview(set_name, tasks, models, n)
            continue
        if not args.summarize:
            if set_name == "local":
                with SiteServer() as base:
                    run_set(set_name, tasks, models, n, {**env, "EVAL_BASE": base})
            else:
                run_set(set_name, tasks, models, n, env)
        write_summary(set_name, tasks, models, n)
    return 0


if __name__ == "__main__":
    sys.exit(main())
