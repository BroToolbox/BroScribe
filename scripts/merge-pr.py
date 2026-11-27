#!/usr/bin/env python3
"""Merge one pull request from the trusted BroScribe merge workflow."""

import argparse
import json
import re
import subprocess

REPOSITORY = "BroToolbox/BroScribe"
MAINTAINER_IDS = frozenset({4291485, 25078493})
DECISIVE_STATES = frozenset({"APPROVED", "CHANGES_REQUESTED", "DISMISSED"})
GITHUB_ACTIONS_APP_ID = 15368


def verify_policy(pull, reviews):
    """Return the exact head eligible for merge, or explain the refusal."""
    if (
        pull["base"]["ref"] != "main"
        or pull["base"]["repo"]["full_name"].lower() != REPOSITORY.lower()
    ):
        raise ValueError("The pull request must target BroToolbox/BroScribe main.")
    if pull["state"] != "open" or pull["draft"]:
        raise ValueError("The pull request must be open and ready for review.")

    head = pull["head"]["sha"]
    if not re.fullmatch(r"[0-9a-f]{40}", head):
        raise ValueError("GitHub did not return a valid pull request head.")
    head_repo = (pull["head"].get("repo") or {}).get("full_name", "")
    if pull["user"]["id"] in MAINTAINER_IDS and head_repo.lower() == REPOSITORY.lower():
        # Only branches in this repository: a fork branch can gain commits
        # from someone else after a maintainer opens the pull request.
        return head

    latest = {}
    for review in sorted(reviews, key=lambda review: review["id"]):
        reviewer = review.get("user") or {}
        if reviewer.get("id") in MAINTAINER_IDS and review["state"] in DECISIVE_STATES:
            latest[reviewer["id"]] = review
    if any(review["state"] == "CHANGES_REQUESTED" for review in latest.values()):
        raise ValueError("A maintainer has outstanding requested changes.")
    if any(
        review["state"] == "APPROVED" and review["commit_id"] == head
        for review in latest.values()
    ):
        return head
    raise ValueError("A maintainer must approve the current pull request head.")


def verify_branch_rules(rules):
    if not any(rule["type"] == "pull_request" for rule in rules):
        raise ValueError("Configure the active main pull request ruleset before merging.")
    if not any(rule["type"] == "update" for rule in rules):
        raise ValueError("Configure the active main update restriction before merging.")
    for rule in rules:
        if rule["type"] != "required_status_checks":
            continue
        parameters = rule.get("parameters") or {}
        if parameters.get("strict_required_status_checks_policy") and any(
            check["context"] == "BroScribe CI"
            and check.get("integration_id") == GITHUB_ACTIONS_APP_ID
            for check in parameters.get("required_status_checks", [])
        ):
            return
    raise ValueError("Configure up-to-date BroScribe CI from GitHub Actions before merging.")


def github_api(path, *args):
    result = subprocess.run(
        ["gh", "api", path, *args], check=True, capture_output=True, text=True
    )
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pull_request", type=int)
    args = parser.parse_args()
    if args.pull_request < 1:
        parser.error("the pull request number must be positive")
    path = f"repos/{REPOSITORY}/pulls/{args.pull_request}"
    try:
        pull = github_api(path)
        pages = github_api(f"{path}/reviews?per_page=100", "--paginate", "--slurp")
        reviews = [review for page in pages for review in page]
        head = verify_policy(pull, reviews)
        verify_branch_rules(github_api(f"repos/{REPOSITORY}/rules/branches/main"))
        result = github_api(
            f"{path}/merge", "--method", "PUT", "--raw-field", f"sha={head}",
            "--raw-field", "merge_method=squash",
        )
        if result["merged"] is not True:
            raise ValueError("GitHub refused to merge the pull request.")
    except subprocess.CalledProcessError as error:
        parser.exit(1, f"GitHub refused the request. Check required CI and branch rules.\n{error.stderr}")
    except ValueError as error:
        parser.exit(1, f"{error}\n")
    print(f"Merged pull request #{args.pull_request} at {result['sha']}.")


if __name__ == "__main__":
    main()
