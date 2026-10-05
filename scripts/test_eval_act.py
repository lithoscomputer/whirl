"""Tests for scripts/eval_act.py. They read small report fixtures and call no
model and no browser."""
import unittest

import eval_act


def step(status="passed", code=None, act=None, duration_ms=1200):
    step = {"line": 5, "kind": "action", "text": "ACT", "status": status, "durationMs": duration_ms}
    if code is not None:
        step["error"] = {"code": code, "message": f"{code}: detail"}
    if act is not None:
        step["act"] = act
    return step


def act_report(calls=1, input_tokens=3000, output_tokens=150, cost=7500, planned_by=()):
    usage = {"modelCalls": calls, "inputTokens": input_tokens, "outputTokens": output_tokens}
    if cost is not None:
        usage["costUsdMicros"] = cost
    actions = [{"line": "CLICK role:button", "description": "", "plannedBy": planner} for planner in planned_by]
    return {"model": "m", "actions": actions, "usage": usage}


def file_report(path, status, entries):
    return {"path": path, "status": status, "entries": entries}


def entry(name, status, steps):
    return {"name": name, "status": status, "steps": steps}


class ClassifyTest(unittest.TestCase):
    def test_a_passing_flow_is_a_pass_with_the_act_measurements(self):
        report = file_report(
            "evals/act/local/flows/custom-dropdown.whirl",
            "passed",
            [entry("Choose.", "passed", [step(), step(act=act_report(calls=2))])],
        )
        result = eval_act.classify(report, "m")
        self.assertEqual(result.task, "custom-dropdown")
        self.assertEqual((result.actions, result.jev_actions), (0, 0))
        self.assertEqual(result.outcome, "pass")
        self.assertEqual(result.model_calls, 2)
        self.assertEqual(result.cost_usd_micros, 7500)
        self.assertTrue(result.priced)

    def test_a_task_with_several_act_lines_sums_their_measurements(self):
        report = file_report(
            "x/saved-checkboxes.whirl",
            "passed",
            [entry("Two.", "passed", [step(act=act_report(), duration_ms=1000), step(act=act_report(calls=2, cost=500), duration_ms=3000)])],
        )
        result = eval_act.classify(report, "m")
        self.assertEqual(result.duration_ms, 4000)
        self.assertEqual(result.model_calls, 3)
        self.assertEqual(result.input_tokens, 6000)
        self.assertEqual(result.cost_usd_micros, 8000)
        unpriced = file_report("x/a.whirl", "passed", [entry("Two.", "passed", [step(act=act_report()), step(act=act_report(cost=None))])])
        self.assertIsNone(eval_act.classify(unpriced, "m").cost_usd_micros)

    def test_actions_jev_chose_are_counted(self):
        report = file_report(
            "x/saved-google-search.whirl",
            "passed",
            [entry("Two.", "passed", [step(act=act_report(planned_by=("jev",))), step(act=act_report(planned_by=("llm",)))])],
        )
        result = eval_act.classify(report, "jev:m")
        self.assertEqual((result.actions, result.jev_actions), (2, 1))

    def test_a_step_without_model_calls_costs_nothing(self):
        report = file_report("x/a.whirl", "passed", [entry("One.", "passed", [step(act=act_report(calls=0, cost=None))])])
        result = eval_act.classify(report, "jev:m")
        self.assertTrue(result.priced)
        self.assertEqual(result.cost_usd_micros, 0)

    def test_a_failing_flow_is_a_fail_with_its_error_code(self):
        report = file_report(
            "x/native-select.whirl",
            "failed",
            [entry("Choose.", "failed", [step(), step("failed", "act-invalid-decision", act_report())])],
        )
        result = eval_act.classify(report, "m")
        self.assertEqual(result.outcome, "fail")
        self.assertEqual(result.code, "act-invalid-decision")

    def test_a_runtime_error_is_a_harness_error(self):
        report = file_report("x/login.whirl", "error", [entry("Fill.", "error", [step("error", "act-model")])])
        self.assertEqual(eval_act.classify(report, "m").outcome, "error")

    def test_a_failed_precheck_is_drift(self):
        report = file_report(
            "x/wikipedia.whirl",
            "failed",
            [
                entry("precheck: the article is present.", "failed", [step("failed", "assert")]),
                entry("Follow a link.", "skipped", [step("skipped")]),
            ],
        )
        self.assertEqual(eval_act.classify(report, "m").outcome, "drift")

    def test_a_no_match_task_passes_only_on_act_no_match(self):
        path = "x/settings-link.no-match.whirl"
        no_match = file_report(path, "failed", [entry("Open.", "failed", [step("failed", "act-no-match")])])
        chose = file_report(path, "passed", [entry("Open.", "passed", [step(act=act_report())])])
        self.assertEqual(eval_act.classify(no_match, "m").task, "settings-link")
        self.assertEqual(eval_act.classify(no_match, "m").outcome, "pass")
        self.assertEqual(eval_act.classify(chose, "m").outcome, "fail")


class PlanTest(unittest.TestCase):
    def test_drift_and_errors_do_not_count_toward_n(self):
        tasks = [eval_act.Task("local", "a", None, False), eval_act.Task("local", "b", None, False)]
        results = [
            eval_act.Result("a", "m", "pass"),
            eval_act.Result("a", "m", "fail"),
            eval_act.Result("b", "m", "error"),
            eval_act.Result("b", "m", "drift"),
        ]
        self.assertEqual(eval_act.shortfalls(tasks, ["m"], results, 2), {"m": {"b": 2}})
        self.assertEqual(eval_act.shortfalls(tasks[:1], ["m"], results, 2), {})

    def test_model_slugs_are_path_safe(self):
        self.assertEqual(eval_act.slug("gemini/gemini-3.1-flash-lite"), "gemini-gemini-3.1-flash-lite")
        self.assertEqual(eval_act.slug("jev:gemini/gemini-3.1-flash-lite"), "jev-gemini-gemini-3.1-flash-lite")

    def test_a_jev_model_runs_with_the_jev_flag(self):
        self.assertEqual(eval_act.whirl_model("jev:gemini/gemini-3.1-flash-lite"), ("gemini/gemini-3.1-flash-lite", ["--jev"]))
        self.assertEqual(eval_act.whirl_model("openrouter/gpt-5.6-luna"), ("openrouter/gpt-5.6-luna", []))


class SummaryTest(unittest.TestCase):
    def render(self, results, models=("m",)):
        tasks = [eval_act.Task("local", "a", None, False), eval_act.Task("local", "b", None, False)]
        return eval_act.render_summary("local", tasks, list(models), results, 2, "2026-09-26", "whirl 0.12.0", "abc1234")

    def measured(self, task, outcome, cost=7500, code=None):
        result = eval_act.Result(task, "m", outcome, code=code)
        result.duration_ms, result.model_calls = 2000, 1
        result.input_tokens, result.output_tokens = 3000, 150
        result.cost_usd_micros, result.priced = cost, cost is not None
        return result

    def test_the_leaderboard_reports_the_pass_rate_timing_tokens_and_cost(self):
        text = self.render([self.measured("a", "pass"), self.measured("b", "fail", code="assert")])
        self.assertIn("| `m` | 0.50 ± 0.35 | 2 | 0 | 0 | 2.0s | 2.0s | 1.00 | - | 3.0k | 150 | $0.0075 | $0.0150 |", text)
        self.assertIn("| a | 1/1 |", text)
        self.assertIn("| b | 0/1 |", text)
        self.assertIn("- `m`: `assert` 1", text)
        self.assertIn("commit `abc1234`", text)

    def test_cost_is_not_applicable_when_any_run_is_unpriced(self):
        text = self.render([self.measured("a", "pass"), self.measured("b", "pass", cost=None)])
        self.assertIn("| n/a | n/a |", text)

    def test_the_leaderboard_shows_the_share_of_actions_jev_chose(self):
        tasks = [eval_act.Task("local", "a", None, False)]
        result = self.measured("a", "pass")
        result.model = "jev:m"
        result.actions, result.jev_actions = 4, 3
        text = eval_act.render_summary("local", tasks, ["jev:m"], [result], 1, "2026-09-27", "whirl", "abc")
        self.assertIn("| `jev:m` | 1.00 ± 0.00 | 1 | 0 | 0 | 2.0s | 2.0s | 1.00 | 0.75 |", text)

    def test_a_model_without_good_runs_shows_no_rate(self):
        text = self.render([eval_act.Result("a", "m", "error")])
        self.assertIn("| `m` | n/a | 0 | 0 | 1 |", text)


if __name__ == "__main__":
    unittest.main()
