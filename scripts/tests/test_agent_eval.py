"""Offline tests of evaluation tooling; synthetic traces are not model results."""

import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("agent_eval", ROOT / "scripts/agent_eval.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def events(*values):
    return [json.dumps(value) + "\n" for value in values]


def command(identity="c1", text="cargo test", code=0):
    return {"type": "item.completed", "item": {
        "id": identity, "type": "command_execution", "command": text, "exit_code": code,
    }}


def completed(*items, usage=None):
    return events({"type": "thread.started", "thread_id": "test"},
                  {"type": "turn.started"}, *items,
                  {"type": "turn.completed", "usage": usage})


class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.cases = MODULE.load_cases()
        self.case = copy.deepcopy(self.cases[0])
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "cases.json"

    def write(self, cases):
        self.path.write_text(json.dumps({"version": 1, "cases": cases}), encoding="utf-8")
        return self.path

    def test_catalog_contains_all_activation_modes(self):
        self.assertTrue({"T01", "T03", "T13", "T14", "T16", "T17", "T18", "T20"}
                        <= {case["id"] for case in self.cases})
        self.assertEqual({case["mode"] for case in self.cases}, {"implicit", "explicit", "negative"})

    def test_prompt_does_not_include_rubric_or_setup(self):
        self.case["fixture"]["setup"] = "PRIVATE FIXTURE SENTINEL"
        self.case["checks"] = ["PRIVATE GRADING SENTINEL"]
        self.assertEqual(MODULE.prompt_for([self.case], self.case["id"]), self.case["prompt"])

    def test_unknown_case_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.prompt_for(self.cases, "T99")

    def test_duplicate_ids_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.load_cases(self.write([self.case, self.case]))

    def test_empty_checks_rejected(self):
        self.case["checks"] = []
        with self.assertRaises(ValueError):
            MODULE.load_cases(self.write([self.case]))

    def test_explicit_case_requires_named_skill(self):
        self.case["mode"] = "explicit"
        with self.assertRaises(ValueError):
            MODULE.load_cases(self.write([self.case]))

    def test_controlled_fixture_requires_setup(self):
        self.case["fixture"].update(kind="controlled", setup="")
        with self.assertRaises(ValueError):
            MODULE.load_cases(self.write([self.case]))

    def test_nonportable_paths_rejected(self):
        for path in ("/tmp/file", "../secret", "a/../b", "C:/file", "a\\b", ".", "a//b"):
            with self.subTest(path=path):
                self.case["fixture"]["paths"] = [path]
                with self.assertRaises(ValueError):
                    MODULE.load_cases(self.write([self.case]))

    def test_unknown_fields_rejected(self):
        self.case["cheks"] = ["typo"]
        with self.assertRaises(ValueError):
            MODULE.load_cases(self.write([self.case]))


class TraceTests(unittest.TestCase):
    def test_completed_trace_is_not_a_behavioral_pass(self):
        result = MODULE.summarize(completed(command()), 0)
        self.assertEqual(result["trace_status"], "completed")
        self.assertEqual(result["behavioral_grade"], "NOT_GRADED")
        self.assertEqual(result["command_count"], 1)

    def test_counts_terminal_commands_not_updates(self):
        item = command()["item"]
        lines = completed({"type": "item.started", "item": item},
                          {"type": "item.updated", "item": item}, command())
        self.assertEqual(MODULE.summarize(lines, 0)["command_count"], 1)

    def test_repeated_strings_are_observations_not_redundancy_verdicts(self):
        result = MODULE.summarize(completed(command(), command("c2")), 0)
        self.assertEqual(result["repeated_command_strings"], {"cargo test": 2})
        self.assertNotIn("redundant_checks", result)

    def test_command_text_is_never_executed(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "must-not-exist"
            result = MODULE.summarize(completed(command(text="touch " + str(marker))), 0)
            self.assertFalse(marker.exists())
            self.assertEqual(result["commands"][0]["command"], "touch " + str(marker))

    def test_failed_command_can_precede_completed_repair(self):
        result = MODULE.summarize(completed(command(code=1), command("c2")), 0)
        self.assertEqual(result["trace_status"], "completed")
        self.assertEqual(result["failed_command_count"], 1)
        self.assertEqual(result["behavioral_grade"], "NOT_GRADED")

    def test_missing_command_status_is_not_success(self):
        result = MODULE.summarize(completed(command(code=None)), 0)
        self.assertEqual(result["unknown_command_exit_count"], 1)
        self.assertIsNone(result["commands"][0]["exit_code"])

    def test_nonzero_process_exit_overrides_completion(self):
        self.assertEqual(MODULE.summarize(completed(), 1)["trace_status"], "failed")

    def test_failed_turn_is_not_overwritten_by_later_completion(self):
        lines = events({"type": "turn.started"}, {"type": "turn.failed"}) + completed()
        self.assertEqual(MODULE.summarize(lines, 0)["trace_status"], "failed")

    def test_empty_or_truncated_trace_is_incomplete(self):
        for lines in ([], events({"type": "turn.started"}),
                      completed() + events({"type": "turn.started"}),
                      events({"type": "turn.completed"})):
            with self.subTest(lines=lines):
                self.assertEqual(MODULE.summarize(lines, 0)["trace_status"], "incomplete")

    def test_items_after_completion_are_not_a_complete_lifecycle(self):
        lines = completed() + events(command())
        result = MODULE.summarize(lines, 0)
        self.assertEqual(result["trace_status"], "incomplete")
        self.assertIn("item outside active turn", result["lifecycle_gaps"])

    def test_empty_item_identity_is_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.summarize(completed(command(identity="")), 0)

    def test_unfinished_item_is_incomplete(self):
        lines = completed({"type": "item.started", "item": command()["item"]})
        self.assertEqual(MODULE.summarize(lines, 0)["trace_status"], "incomplete")

    def test_invalid_json_and_nonobject_events_rejected(self):
        for lines in (['{'], ["[]"], ["null"], ['{"type": 7}']):
            with self.subTest(lines=lines):
                with self.assertRaises(ValueError):
                    MODULE.summarize(lines, 0)

    def test_duplicate_terminal_item_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.summarize(completed(command(), command()), 0)

    def test_unknown_event_types_remain_visible(self):
        result = MODULE.summarize(completed({"type": "future.event"}), 0)
        self.assertEqual(result["unknown_event_types"], ["future.event"])
        self.assertEqual(result["behavioral_grade"], "NOT_GRADED")

    def test_usage_is_summed_across_turns(self):
        lines = completed(usage={"input_tokens": 10, "cached_input_tokens": 2, "output_tokens": 3})
        lines += completed(usage={"input_tokens": 7, "cached_input_tokens": 1, "output_tokens": 4})
        self.assertEqual(MODULE.summarize(lines, 0)["usage"], {
            "input_tokens": 17, "cached_input_tokens": 3, "output_tokens": 7,
        })

    def test_missing_usage_is_unknown_not_zero(self):
        lines = completed(usage={"input_tokens": 10}) + completed()
        self.assertEqual(MODULE.summarize(lines, 0)["usage"], {
            "input_tokens": None, "cached_input_tokens": None, "output_tokens": None,
        })

    def test_negative_boolean_or_string_token_counts_rejected(self):
        for value in (-1, True, "12"):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    MODULE.summarize(completed(usage={"input_tokens": value}), 0)


class CommandLineTests(unittest.TestCase):
    def test_validate_is_offline(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = MODULE.main(["validate"])
        self.assertEqual(status, 0)
        self.assertIn("no model runs performed", output.getvalue())

    def test_prompt_contains_only_request(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = MODULE.main(["prompt", "T13"])
        self.assertEqual(status, 0)
        self.assertEqual(output.getvalue(), MODULE.prompt_for(MODULE.load_cases(), "T13") + "\n")

    def test_bad_catalog_returns_error_without_traceback(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(MODULE.main(["--catalog", "/nonexistent/catalog.json", "validate"]), 2)

    def test_failed_trace_reports_failure_and_preserves_json(self):
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "trace.jsonl"
            trace.write_text("".join(completed()), encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                status = MODULE.main(["summarize", str(trace), "--exit-code", "7"])
            self.assertEqual(status, 1)
            self.assertEqual(json.loads(output.getvalue())["trace_status"], "failed")


if __name__ == "__main__":
    unittest.main()
