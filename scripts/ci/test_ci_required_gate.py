import unittest

from required_gate import ALL_JOBS, FULL_JOBS, TARGETED_JOBS, validate


def result_set(full=False, docs_build=False, windows=False, installer=False):
    needs = {job: {"result": "skipped", "outputs": {}} for job in ALL_JOBS}
    needs["changes"] = {"result": "success", "outputs": {
        "full": str(full).lower(),
        "docs_build": str(docs_build).lower(),
        "windows_clippy": str(windows).lower(),
        "installer": str(installer).lower(),
    }}
    needs["docs-style"]["result"] = "success"
    if full:
        for job in FULL_JOBS:
            needs[job]["result"] = "success"
        if windows:
            needs["windows-clippy-tools"]["result"] = "success"
        if installer:
            needs["installer-drift"]["result"] = "success"
    if docs_build:
        needs["docs-build"]["result"] = "success"
    return needs


class GateTests(unittest.TestCase):
    def test_accepts_prose_and_code_expected_results_on_each_event(self):
        for event in ("pull_request", "push", "merge_group"):
            for full, build in ((False, False), (False, True), (True, False), (True, True)):
                with self.subTest(event=event, full=full, build=build):
                    self.assertEqual(validate(result_set(full, build), event), [])

    def test_accepts_selected_targeted_jobs(self):
        for windows, installer in ((True, False), (False, True), (True, True)):
            with self.subTest(windows=windows, installer=installer):
                needs = result_set(full=True, windows=windows, installer=installer)
                self.assertEqual(validate(needs, "pull_request"), [])

    def test_targeted_jobs_only_run_on_the_full_lane(self):
        needs = result_set(windows=True, installer=True)
        self.assertEqual(validate(needs, "pull_request"), [])

    def test_failure_cancellation_or_unexpected_skip_cannot_pass(self):
        for job in ("docs-style", "lint", "test", "policy", "check", "build"):
            for result in ("failure", "cancelled", "skipped", "neutral", None):
                with self.subTest(job=job, result=result):
                    needs = result_set(full=True)
                    needs[job]["result"] = result
                    self.assertTrue(validate(needs, "pull_request"))

    def test_selected_jobs_are_required(self):
        needs = result_set(docs_build=True)
        needs["docs-build"]["result"] = "skipped"
        self.assertTrue(validate(needs, "pull_request"))
        for job in TARGETED_JOBS:
            with self.subTest(job=job):
                needs = result_set(full=True, windows=True, installer=True)
                needs[job]["result"] = "skipped"
                self.assertTrue(validate(needs, "pull_request"))

    def test_classifier_failure_or_missing_output_is_fatal(self):
        for status in ("failure", "cancelled", "skipped"):
            needs = result_set()
            needs["changes"]["result"] = status
            self.assertTrue(validate(needs, "pull_request"))
        for output in ("full", "docs_build"):
            for value in (None, "", "True", True):
                needs = result_set()
                needs["changes"]["outputs"][output] = value
                self.assertTrue(validate(needs, "pull_request"))

    def test_targeted_selection_output_is_required_for_full_lane(self):
        for output in TARGETED_JOBS.values():
            with self.subTest(output=output):
                needs = result_set(full=True)
                del needs["changes"]["outputs"][output]
                self.assertTrue(validate(needs, "pull_request"))

    def test_missing_or_added_dependencies_cannot_silently_pass(self):
        for job in ALL_JOBS:
            needs = result_set()
            del needs[job]
            self.assertTrue(validate(needs, "pull_request"))
        needs = result_set()
        needs["unclassified"] = {"result": "success"}
        self.assertTrue(validate(needs, "pull_request"))

    def test_unselected_failure_is_not_ignored(self):
        needs = result_set()
        needs["lint"]["result"] = "failure"
        self.assertTrue(validate(needs, "pull_request"))


if __name__ == "__main__":
    unittest.main()
