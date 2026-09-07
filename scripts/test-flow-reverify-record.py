#!/usr/bin/env python3
"""What the canary WRITES, with git, gh and the phone all faked.

Verdict classification is only half the job. The half that can damage the
registry is `record()`: it edits flow files, tags them, opens a PR, and files
issues. This drives that path offline and pins three properties:

  * a night where nothing was judged changes nothing at all — no branch, no
    push, no PR, no issue;
  * a skipped flow's file is byte-for-byte identical afterwards;
  * nothing free-form reaches the public PR body.

No phone, no network, no real repository.
"""
import importlib.util
import json
import os
import pathlib
import shutil
import sys
import tempfile

MODULE = pathlib.Path(__file__).resolve().parent / "flow-reverify.py"
spec = importlib.util.spec_from_file_location("flow_reverify", MODULE)
reverify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reverify)

FAILURES = []


def check(name, got, want):
    if got != want:
        FAILURES.append(f"{name}: got {got!r}, want {want!r}")


def ok(name, condition, detail=""):
    if not condition:
        FAILURES.append(f"{name}{': ' + detail if detail else ''}")


class FakeWorld:
    """Stands in for gh, git and the MCP binary."""

    def __init__(self, flows):
        self.calls = []
        self.reports = []
        self.flows = flows

    def sh(self, args, cwd=None, check=True):
        self.calls.append(list(args))
        if args[:3] == ["gh", "repo", "clone"]:
            repo = pathlib.Path(args[4])
            assert repo.is_absolute(), f"the clone destination must be absolute: {repo}"
            repo.mkdir(parents=True, exist_ok=True)
            for flow_id, doc in self.flows.items():
                path = repo / (flow_id + ".json")
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(doc, ensure_ascii=False, indent=2) + "\n", "utf-8")
            (repo / "scripts").mkdir(exist_ok=True)
            (repo / "scripts" / "build-index.py").write_text("", "utf-8")
            return ""
        if args[:3] == ["gh", "pr", "create"]:
            self.pr_body = args[args.index("--body") + 1]
            return "https://github.test/pr/1"
        return ""

    def mcp(self, *args, timeout=None):
        self.reports.append(list(args))
        return 0, "", ""


def install(world, repo_files):
    reverify.sh = world.sh
    reverify.mcp = world.mcp
    reverify.log = lambda *a, **k: None
    shutil.which = lambda name: "/usr/bin/" + name


def flow_doc(name):
    return {"version": 1, "name": name, "steps": [{"kind": "pause", "ms": 1}], "tags": []}


def main():
    # Everything this test writes must land in a temp directory. A fake `gh
    # repo clone` that takes a RELATIVE destination would otherwise create
    # `<owner>/<repo>/` inside whatever checkout the test ran from — which is
    # exactly what happened once.
    sandbox = tempfile.mkdtemp(prefix="flow-reverify-test-")
    os.chdir(sandbox)

    verified, failed, skipped = reverify.VERIFIED, reverify.FAILED, reverify.SKIPPED

    # ---- a night where nothing was judged ----
    flows = {"test/a": flow_doc("A"), "test/b": flow_doc("B")}
    world = FakeWorld(flows)
    install(world, flows)
    results = [
        ({"id": "test/a"}, skipped, "phone unavailable (device_locked)", {}),
        ({"id": "test/b"}, skipped, "unknown outcome", {}),
    ]
    reverify.record(results, "iPhone", "26.0", None)
    pushed = [c for c in world.calls if "push" in c]
    prs = [c for c in world.calls if c[:3] == ["gh", "pr", "create"]]
    ok("an all-skipped night pushes nothing", not pushed, str(pushed))
    ok("an all-skipped night opens no PR", not prs, str(prs))
    ok("an all-skipped night files no issue", not world.reports, str(world.reports))

    # ---- a mixed night ----
    flows = {"test/a": flow_doc("A"), "test/b": flow_doc("B"), "test/c": flow_doc("C")}
    world = FakeWorld(flows)
    install(world, flows)
    before = json.dumps(flows["test/c"], ensure_ascii=False, indent=2) + "\n"
    results = [
        ({"id": "test/a"}, verified, "", {"ok": True}),
        (
            {"id": "test/b"},
            failed,
            "step failed (expectation_timeout) at step 2",
            {"ok": False, "error": "expectation_timeout", "failed_step": 2},
        ),
        ({"id": "test/c"}, skipped, "phone unavailable (phone_owned)", {}),
    ]
    work = None
    original_mkdtemp = tempfile.mkdtemp

    def capture(**kwargs):
        nonlocal work
        work = original_mkdtemp(**kwargs)
        return work

    tempfile.mkdtemp = capture
    reverify.record(results, "iPhone 17", "26.0", None)
    tempfile.mkdtemp = original_mkdtemp

    repo = pathlib.Path(work) / "repo"
    after_a = json.loads((repo / "test" / "a.json").read_text("utf-8"))
    after_b = json.loads((repo / "test" / "b.json").read_text("utf-8"))
    after_c = (repo / "test" / "c.json").read_text("utf-8")

    ok("the verified flow got a date", after_a.get("verified_on"), str(after_a))
    ok(
        "the failed flow is tagged",
        "needs-verification" in after_b.get("tags", []),
        str(after_b),
    )
    check("the skipped flow is byte-identical", after_c, before)
    ok("only the failed flow is reported", len(world.reports) == 1, str(world.reports))

    # ---- nothing free-form reaches the public body ----
    world = FakeWorld({"test/a": flow_doc("A")})
    install(world, world.flows)
    leaky = {
        "ok": False,
        "error": "secret123",
        "failed_step": "http://daemon.invalid:8100",
        "settle": {"error": "GET /source failed for SCREEN-LABEL-CANARY"},
    }
    verdict, reason = reverify.classify(1, leaky)
    check("an unrecognised code is not called a flow failure", verdict, skipped)
    ok("no token in the reason", "secret123" not in reason, reason)
    ok("no URL in the reason", "daemon.invalid" not in reason, reason)
    ok("no label in the reason", "SCREEN-LABEL-CANARY" not in reason, reason)

    reverify.record(
        [({"id": "test/a"}, failed, reverify.public_reason("step failed", "secret123", "9"), leaky)],
        "iPhone",
        "26.0",
        None,
    )
    body = getattr(world, "pr_body", "")
    ok("no token in the PR body", "secret123" not in body, body)
    ok("no URL in the PR body", "daemon.invalid" not in body, body)
    ok("no label in the PR body", "SCREEN-LABEL-CANARY" not in body, body)

    # ---- a dry run sends nothing and still releases nothing ----
    posts = []
    reverify.http = lambda method, path, *a, **k: posts.append((method, path)) or {}
    reverify.preflight = lambda: ({"drivable": True}, None)
    reverify.bring_up = lambda st: True
    reverify.installed_versions = lambda: {"device": "iPhone", "ios": "26.0"}
    reverify.canary_flows = lambda only: [{"id": "test/a", "example_inputs": {}}]
    reverify.TOKEN = "test-token"
    reverify.MCP = __file__  # exists, so the binary check passes

    class Args:
        cmd = "run"
        dry_run = True
        only = ""
        device = ""

    world = FakeWorld({"test/a": flow_doc("A")})
    install(world, world.flows)
    reverify.run(Args())

    ok("a dry run sends no control POST", not posts, str(posts))
    ok("a dry run runs no flow", not world.reports, str(world.reports))
    ok("a dry run touches no repository", not world.calls, str(world.calls))

    if FAILURES:
        print("FAIL")
        for failure in FAILURES:
            print(f"  {failure}")
        return 1
    print("ok: skipped nights change nothing; public bodies carry no free text")
    return 0


if __name__ == "__main__":
    sys.exit(main())
