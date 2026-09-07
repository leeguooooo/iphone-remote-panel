#!/usr/bin/env python3
"""The nightly canary must not answer a question the night never asked.

A run that never happened — the phone was locked, someone else held the owner
lease, WDA was down, the daemon could not determine the outcome — says nothing
about the flow. Recording it as a failure files a `flow-broken` issue against a
flow that is probably fine and tags it `needs-verification`, which stops the
registry recommending it. Recording it as a pass is worse: it would refresh
`verified_on` for a run that did not occur.
"""
import importlib.util
import pathlib
import sys

MODULE = pathlib.Path(__file__).resolve().parent / "flow-reverify.py"
spec = importlib.util.spec_from_file_location("flow_reverify", MODULE)
reverify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reverify)

FAILURES = []


def check(name, got, want):
    if got != want:
        FAILURES.append(f"{name}: got {got!r}, want {want!r}")


def main():
    verified, failed, skipped = reverify.VERIFIED, reverify.FAILED, reverify.SKIPPED

    # A clean pass.
    check(
        "successful run is verified",
        reverify.classify(0, {"ok": True, "completed": 4})[0],
        verified,
    )

    # A flow that genuinely broke: somebody SAW the screen, and the condition
    # was not met on it.
    seen = {"read": True, "sparse": False, "stale": None, "missing_present": [0]}
    verdict, reason = reverify.classify(1, {
        "ok": False, "error": "expectation_timeout", "failed_step": 3,
        "applied_actions": 2, "outcome": "not_sent", "observation": seen,
    })
    check("a real step failure is failed", verdict, failed)
    check("the reason names the step", "step 3" in reason, True)

    # The same error code, but nobody could see the screen. That is the
    # daemon saying "I do not know", and it must not tag the flow.
    for name, observation in (
        ("never read", {"read": False, "reads": 0}),
        ("stale last read", {"read": True, "stale": True}),
        ("bare tree", {"read": True, "sparse": True, "absent_unproven": [0]}),
        ("no observation at all", None),
    ):
        verdict, reason = reverify.classify(1, {
            "ok": False, "error": "expectation_timeout", "failed_step": 3,
            "observation": observation,
        })
        check(f"expectation_timeout with {name} is skipped", verdict, skipped)
        check(f"{name} reason says the screen was not readable",
              "screen not readable" in reason, True)

    # A batch that timed out before doing anything never started.
    check(
        "a batch deadline with nothing done is skipped",
        reverify.classify(1, {
            "ok": False, "error": "batch_deadline", "completed": 0, "applied_actions": 0,
        })[0],
        skipped,
    )
    check(
        "a batch deadline after real work is a failure",
        reverify.classify(1, {
            "ok": False, "error": "batch_deadline_after_action",
            "completed": 2, "applied_actions": 2,
        })[0],
        failed,
    )

    # Wrongly typed verdict fields are rejected before anything is concluded.
    for bad in ({}, [], "yes", 1):
        check(
            f"ok={bad!r} is not a verdict",
            reverify.classify(0, {"ok": bad})[0],
            skipped,
        )
    check(
        "a non-string outcome is not a verdict",
        reverify.classify(0, {"ok": True, "outcome": {}})[0],
        skipped,
    )

    # The phone was not available. Not the flow's fault, and not a pass.
    for error in (
        "device_not_drivable",
        "device_locked",
        "phone_owned",
        "phone_handed_to_human",
        "wda_source_failed",
        "device_transition_in_progress",
    ):
        verdict, reason = reverify.classify(1, {"ok": False, "error": error})
        check(f"{error} is skipped", verdict, skipped)
        check(f"{error} reason names the cause", error in reason, True)

    # The daemon dispatched something and cannot say what happened. Calling the
    # flow broken on that basis would be a guess.
    verdict, reason = reverify.classify(1, {
        "ok": False, "error": "outcome_unknown", "outcome": "unknown", "retry_safe": False,
    })
    check("an unknown outcome is skipped", verdict, skipped)
    check("the reason says why", "unknown" in reason, True)

    # Nothing machine-readable came back at all.
    check(
        "an unparseable result is skipped",
        reverify.classify(1, {"raw": "connection refused"})[0],
        skipped,
    )
    check("a non-dict result is skipped", reverify.classify(1, None)[0], skipped)

    # Success that carries an error field, of any type, is a contradiction —
    # not a pass. Dropping a wrongly-typed field and reading `ok:true` would
    # refresh a verification date on a result nobody understands.
    for bad in ("expectation_timeout", {"code": "x"}, ["x"], 7):
        check(
            f"ok:true alongside error={bad!r} is skipped",
            reverify.classify(0, {"ok": True, "error": bad})[0],
            skipped,
        )
    check(
        "ok:true with a contradicting outcome is skipped",
        reverify.classify(0, {"ok": True, "outcome": "not_sent"})[0],
        skipped,
    )
    check(
        "a clean pass with outcome applied is still verified",
        reverify.classify(0, {"ok": True, "outcome": "applied", "completed": 4})[0],
        verified,
    )

    # A non-zero exit with ok:true is not a pass either — the two must agree.
    check(
        "rc and ok must agree",
        reverify.classify(1, {"ok": True})[0],
        skipped,
    )

    if FAILURES:
        print("FAIL")
        for failure in FAILURES:
            print(f"  {failure}")
        return 1
    print("ok: canary verdicts separate verified / failed / skipped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
