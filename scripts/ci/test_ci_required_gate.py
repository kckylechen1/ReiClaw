import unittest

from required_gate import ALL_JOBS, FULL_JOBS, FULL_SELECTION_OUTPUTS, TARGETED_JOBS, validate


def result_set(full=False, installer=False, web=False):
    needs = {job: {"result": "skipped", "outputs": {}} for job in ALL_JOBS}
    needs["changes"] = {"result": "success", "outputs": {
        "full": str(full).lower(),
        "installer": str(installer).lower(),
        "web": str(web).lower(),
    }}
    needs["docs-style"]["result"] = "success"
    if full:
        for job in FULL_JOBS:
            needs[job]["result"] = "success"
        if installer:
            needs["installer-drift"]["result"] = "success"
    return needs


class GateTests(unittest.TestCase):
    def test_accepts_prose_and_code_expected_results_on_each_event(self):
        for event in ("pull_request", "push", "merge_group"):
            for full in (False, True):
                with self.subTest(event=event, full=full):
                    self.assertEqual(validate(result_set(full), event), [])

    def test_accepts_selected_targeted_jobs(self):
        needs = result_set(full=True, installer=True)
        self.assertEqual(validate(needs, "pull_request"), [])

    def test_targeted_jobs_only_run_on_the_full_lane(self):
        needs = result_set(installer=True)
        self.assertEqual(validate(needs, "pull_request"), [])

    def test_failure_cancellation_or_unexpected_skip_cannot_pass(self):
        for job in ("docs-style", "lint", "test", "policy", "check", "build"):
            for result in ("failure", "cancelled", "skipped", "neutral", None):
                with self.subTest(job=job, result=result):
                    needs = result_set(full=True)
                    needs[job]["result"] = result
                    self.assertTrue(validate(needs, "pull_request"))

    def test_selected_jobs_are_required(self):
        for job in TARGETED_JOBS:
            with self.subTest(job=job):
                needs = result_set(full=True, installer=True)
                needs[job]["result"] = "skipped"
                self.assertTrue(validate(needs, "pull_request"))

    def test_classifier_failure_or_missing_output_is_fatal(self):
        for status in ("failure", "cancelled", "skipped"):
            needs = result_set()
            needs["changes"]["result"] = status
            self.assertTrue(validate(needs, "pull_request"))
        for value in (None, "", "True", True):
            needs = result_set()
            needs["changes"]["outputs"]["full"] = value
            self.assertTrue(validate(needs, "pull_request"))

    def test_targeted_selection_output_is_required_for_full_lane(self):
        for output in (*TARGETED_JOBS.values(), *FULL_SELECTION_OUTPUTS):
            with self.subTest(output=output, case="missing"):
                needs = result_set(full=True)
                del needs["changes"]["outputs"][output]
                self.assertTrue(validate(needs, "pull_request"))
            for bad in (None, "", "True", True, "yes"):
                with self.subTest(output=output, case=bad):
                    needs = result_set(full=True)
                    needs["changes"]["outputs"][output] = bad
                    self.assertTrue(validate(needs, "pull_request"))

    def test_web_output_accepted_when_true_or_false_on_full_lane(self):
        for web in (False, True):
            for event in ("pull_request", "push", "merge_group"):
                with self.subTest(web=web, event=event):
                    needs = result_set(full=True, web=web)
                    self.assertEqual(validate(needs, event), [])

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
