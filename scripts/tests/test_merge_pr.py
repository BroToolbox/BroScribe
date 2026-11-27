import unittest

from support import load_script

policy = load_script("merge-pr.py")
HEAD = "a" * 40
MAINTAINER, RENAN = sorted(policy.MAINTAINER_IDS)
CONTRIBUTOR = 123


def pull(author=CONTRIBUTOR, base="main", draft=False, head_repo=None):
    return {
        "state": "open",
        "draft": draft,
        "user": {"id": author},
        "head": {"sha": HEAD, "repo": {"full_name": head_repo or policy.REPOSITORY}},
        "base": {"ref": base, "repo": {"full_name": policy.REPOSITORY}},
    }


def review(reviewer=MAINTAINER, state="APPROVED", commit=HEAD, review_id=1):
    return {
        "id": review_id,
        "user": {"id": reviewer},
        "state": state,
        "commit_id": commit,
    }


class MergePolicyTests(unittest.TestCase):
    def test_both_maintainers_skip_review(self):
        for author in (MAINTAINER, RENAN):
            with self.subTest(author=author):
                self.assertEqual(policy.verify_policy(pull(author), []), HEAD)

    def test_maintainer_pr_from_a_fork_branch_needs_approval(self):
        fork = pull(MAINTAINER, head_repo="someone/BroScribe")
        with self.assertRaisesRegex(ValueError, "current"):
            policy.verify_policy(fork, [])
        self.assertEqual(policy.verify_policy(fork, [review(RENAN)]), HEAD)
        deleted_fork = pull(MAINTAINER)
        deleted_fork["head"]["repo"] = None
        with self.assertRaisesRegex(ValueError, "current"):
            policy.verify_policy(deleted_fork, [])

    def test_external_pr_requires_current_maintainer_approval(self):
        for reviews in ([], [review(CONTRIBUTOR)], [review(commit="b" * 40)]):
            with self.subTest(reviews=reviews):
                with self.assertRaisesRegex(ValueError, "current"):
                    policy.verify_policy(pull(), reviews)
        self.assertEqual(policy.verify_policy(pull(), [review(RENAN)]), HEAD)

    def test_dismissal_replaces_review_and_comment_does_not(self):
        approval = review()
        comment = review(state="COMMENTED", review_id=2)
        self.assertEqual(policy.verify_policy(pull(), [approval, comment]), HEAD)
        dismissal = review(state="DISMISSED", review_id=3)
        with self.assertRaisesRegex(ValueError, "current"):
            policy.verify_policy(pull(), [dismissal, approval])

    def test_requested_changes_blocks_another_maintainers_approval(self):
        with self.assertRaisesRegex(ValueError, "requested changes"):
            policy.verify_policy(pull(), [review(), review(RENAN, "CHANGES_REQUESTED", review_id=2)])

    def test_wrong_target_and_draft_are_refused_even_for_maintainers(self):
        with self.assertRaisesRegex(ValueError, "target"):
            policy.verify_policy(pull(MAINTAINER, base="feature"), [])
        with self.assertRaisesRegex(ValueError, "ready"):
            policy.verify_policy(pull(MAINTAINER, draft=True), [])


    def test_active_pr_and_strict_ci_rules_are_required(self):
        pr = {"type": "pull_request"}
        checks = {
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": True,
                "required_status_checks": [
                    {"context": "BroScribe CI", "integration_id": policy.GITHUB_ACTIONS_APP_ID}
                ],
            },
        }
        update = {"type": "update", "parameters": {"update_allows_fetch_and_merge": False}}
        policy.verify_branch_rules([pr, update, checks])
        with self.assertRaisesRegex(ValueError, "pull request ruleset"):
            policy.verify_branch_rules([update, checks])
        with self.assertRaisesRegex(ValueError, "update restriction"):
            policy.verify_branch_rules([pr, checks])
        with self.assertRaisesRegex(ValueError, "BroScribe CI"):
            policy.verify_branch_rules([pr, update])
        checks["parameters"]["required_status_checks"][0]["integration_id"] = 123
        with self.assertRaisesRegex(ValueError, "GitHub Actions"):
            policy.verify_branch_rules([pr, update, checks])
