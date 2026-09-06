#!/usr/bin/env python3
"""Nightly re-verification of the official flow registry on a real phone.

Runs every installed *canary* flow (official, hardware-verified, risk read_only or
navigation, not tagged `no-canary`, inputs satisfiable from `example_inputs`) through
`iphone-use-mcp flow run` while holding the phone's owner lease, then:

  * success  → refresh that flow's `verified_on` entry for this device with today's date
               and the installed app / iOS version (compat stays `verified`)
  * failure  → `flow report` (a `flow-broken` issue) and tag the flow `needs-verification`
               so `flow list` / `phone_elements` stop recommending it

All registry changes go into ONE pull request (branch `reverify/<date>`) opened with `gh`,
so a human reviews what the night changed. Nothing is merged automatically.

Preconditions (checked, never forced): daemon reachable, `drivable:true`, nobody else owns
the phone, no hold active. Otherwise it logs one line and exits 0 — tomorrow is fine.

Usage:
  flow-reverify.py run [--dry-run] [--only id,id] [--device NAME]
  flow-reverify.py enable|disable|status        # launchd job, daily 03:30

Env: PHONE_REMOTE_URL, PHONE_REMOTE_TOKEN (required for run), IPHONE_USE_MCP (binary),
     IPHONE_USE_FLOWS_REPO (owner/name, default leeguooooo/iphone-use-flows),
     FLOW_REVERIFY_OWNER (owner lease name, default flow-reverify).
"""
import argparse, datetime, json, os, plistlib, shutil, subprocess, sys, tempfile, time, urllib.request

LABEL = "com.leeguoo.iphone-use.flow-reverify"
LOG_DIR = os.path.expanduser("~/Library/Logs/iPhoneUse")
LOG = os.path.join(LOG_DIR, "flow-reverify.log")
REPO = os.environ.get("IPHONE_USE_FLOWS_REPO", "leeguooooo/iphone-use-flows")
OWNER = os.environ.get("FLOW_REVERIFY_OWNER", "flow-reverify")
MCP = os.environ.get("IPHONE_USE_MCP") or os.path.expanduser("~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp")
HOST = os.environ.get("PHONE_REMOTE_URL", "http://127.0.0.1:44321").rstrip("/")
TOKEN = os.environ.get("PHONE_REMOTE_TOKEN", "")


def log(msg):
    os.makedirs(LOG_DIR, exist_ok=True)
    line = f"{datetime.datetime.now().isoformat(timespec='seconds')} {msg}"
    print(line, flush=True)
    with open(LOG, "a", encoding="utf-8") as fh:
        fh.write(line + "\n")


def http(method, path, body=None, control=False, timeout=60):
    req = urllib.request.Request(HOST + path, method=method, data=json.dumps(body).encode() if body is not None else None)
    req.add_header("Authorization", "Bearer " + TOKEN)
    if control:
        req.add_header("X-Phone-Control", "1")
        req.add_header("X-Phone-Owner", OWNER)
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except Exception:
            return e.code, {}
    except Exception as e:  # network
        return 0, {"error": str(e)}


def mcp(*args, env_extra=None, timeout=300):
    env = dict(os.environ, PHONE_REMOTE_OWNER=OWNER)
    env.update(env_extra or {})
    p = subprocess.run([MCP, *args], capture_output=True, text=True, env=env, timeout=timeout)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def sh(args, cwd=None, check=True):
    p = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} failed: {p.stderr.strip() or p.stdout.strip()}")
    return p.stdout.strip()


def preflight():
    code, st = http("GET", "/agent/status", timeout=10)
    if code != 200:
        return None, f"daemon unreachable ({code} {st.get('error','')})"
    if st.get("owner") and st.get("owner") != OWNER:
        return None, f"phone owned by {st['owner']} ({st.get('owner_lease_remaining_secs')}s left)"
    if (st.get("hold_remaining_secs") or 0) > 0:
        return None, f"hold active ({st['hold_remaining_secs']}s)"
    if st.get("device_state") in ("releasing", "reconnecting"):
        return None, f"device_state={st['device_state']}"
    return st, None


def bring_up(st):
    if st.get("drivable"):
        return True
    if st.get("device_state") in ("released", "offline"):
        log(f"device {st['device_state']}; requesting one bring-up")
        http("POST", "/agent/mode", {"mode": "agent"}, control=True, timeout=15)
        for _ in range(36):
            time.sleep(10)
            code, cur = http("GET", "/agent/status", timeout=10)
            if cur.get("drivable"):
                return True
            if cur.get("device_state") in ("locked", "blocked"):
                log(f"bring-up stopped: {cur.get('device_state')} {cur.get('hint','')}")
                return False
    return False


def installed_versions():
    rc, out, _ = mcp("flow", "apps", "--json", timeout=120)
    if rc != 0:
        return None
    try:
        return json.loads(out)
    except Exception:
        return None


def compat_version(apps, bundle):
    if not apps or not bundle:
        return None
    a = (apps.get("apps") or {}).get(bundle)
    if bundle.startswith("com.apple.") and (a is None or a.get("system")):
        return apps.get("ios")
    return (a or {}).get("version")


def canary_flows(only):
    rc, out, err = mcp("flow", "list", "--json", timeout=120)
    if rc != 0:
        raise RuntimeError(f"flow list failed: {err}")
    flows = json.loads(out)["flows"]
    chosen = []
    for f in flows:
        if only and f["id"] not in only:
            continue
        tags = f.get("tags") or []
        if f.get("source") != "official" or not f.get("verified"):
            continue
        if f.get("risk") not in ("read_only", "navigation") or "no-canary" in tags or "broken" in tags:
            continue
        inputs = f.get("inputs") or []
        example = f.get("example_inputs") or {}
        if any(i not in example for i in inputs):
            continue
        chosen.append(f)
    return chosen


# A night on which the phone was unavailable says NOTHING about the flow.
# Treating it as a failure files a `flow-broken` issue against a flow that is
# probably fine and tags it `needs-verification`, which stops `flow list` and
# `phone_elements` recommending it — real damage from an unrelated cause.
# Treating it as a pass is worse: it would refresh `verified_on` for a run that
# never happened. Both are wrong, so it is its own verdict.
ENVIRONMENT_ERRORS = {
    "device_not_drivable",
    "device_locked",
    "device_release_in_progress",
    "device_transition_in_progress",
    "reconnect_in_progress",
    "lifecycle_busy",
    "phone_owned",
    "phone_handed_to_human",
    "wda_not_configured",
    "wda_unavailable_or_unsupported",
    "wda_pre_dispatch_failed",
    "wda_source_failed",
    "wda_source_timeout",
    "backend_is_mirror",
    "target_not_configured",
}

VERIFIED, FAILED, SKIPPED = "verified", "failed", "skipped"

# Reasons that may appear in a PUBLIC pull request body. Same rule as the
# result projection in `contrib.rs`: only daemon-authored codes and numbers get
# published, never free text that could carry a URL, a label, or a token.
PUBLIC_REASONS = {
    "unknown outcome",
    "no machine-readable result",
    "no verdict in the result",
    "result and exit status disagree",
    "phone unavailable",
    "step failed",
    "screen not readable",
    "did not start",
}


def public_reason(kind, code=None, step=None):
    """A reason built only from a known phrase, a known code, and a number."""
    parts = [kind if kind in PUBLIC_REASONS else "unknown reason"]
    if code and code in ENVIRONMENT_ERRORS:
        parts.append(f"({code})")
    elif code and code in KNOWN_FLOW_ERRORS:
        parts.append(f"({code})")
    elif code:
        parts.append("(unrecognised code)")
    if isinstance(step, bool) or not isinstance(step, int):
        step = None
    if step is not None:
        parts.append(f"at step {step}")
    return " ".join(parts)


# Errors that mean the flow itself did not do what it claims. Anything not in
# here and not in ENVIRONMENT_ERRORS is not evidence the flow is broken.
KNOWN_FLOW_ERRORS = {
    "expectation_timeout",
    "element_not_found",
    "ambiguous_element_label",
    "stale_element_snapshot",
    "invalid_element_snapshot",
    "invalid_element_target",
    "unsupported_control",
    "unsupported_perform_action",
    "batch_deadline",
    "batch_deadline_after_action",
}


def classify(rc, result):
    """(verdict, public_reason) for one canary run.

    Order matters. "It worked" is the LAST thing checked, not the first:
    a result that also says `outcome: unknown`, or whose exit status disagrees
    with its body, has not earned a refreshed verification date. And only a
    recognised flow-level failure is called `failed` — an unrecognised error
    code is something we do not understand, not proof the flow is broken.
    """
    if not isinstance(result, dict):
        return SKIPPED, public_reason("no machine-readable result")

    ok = result.get("ok")
    raw_error = result.get("error")
    raw_outcome = result.get("outcome")
    step = result.get("failed_step")

    # Reject wrongly-typed verdict fields up front. Coercing them to None and
    # carrying on is how `{"ok": true, "outcome": {}}` ends up refreshing a
    # verification date.
    if ok is not None and not isinstance(ok, bool):
        return SKIPPED, public_reason("no verdict in the result")
    if raw_outcome is not None and not isinstance(raw_outcome, str):
        return SKIPPED, public_reason("no verdict in the result")
    if raw_error is not None and not isinstance(raw_error, str):
        return SKIPPED, public_reason("no verdict in the result")
    error = raw_error
    outcome = raw_outcome

    # A result that claims success while also carrying an error field — of any
    # type — is not a result we understand. Dropping the field because it was
    # the wrong type and then reading `ok:true` would refresh a verification
    # date on a contradiction.
    if ok is True and (raw_error is not None or (outcome is not None and outcome != "applied")):
        return SKIPPED, public_reason("result and exit status disagree")

    # The daemon dispatched something and cannot say what happened.
    if outcome == "unknown" or error == "outcome_unknown":
        return SKIPPED, public_reason("unknown outcome")
    # The phone was not in a state to answer the question.
    if error in ENVIRONMENT_ERRORS:
        return SKIPPED, public_reason("phone unavailable", error)
    # The exit status and the body must agree before anything is recorded.
    if (rc == 0) != (ok is True):
        return SKIPPED, public_reason("result and exit status disagree")
    if rc == 0 and ok is True:
        return VERIFIED, ""
    # A `wait_for` that timed out is only the flow's fault if somebody could
    # actually SEE the screen. The daemon now says so directly, and this is
    # what that evidence is for: a tree that was never read, a last read that
    # failed, or a tree too bare to prove an `absent` locator all mean the
    # question went unanswered — not that the flow is broken.
    unreadable = observation_is_inconclusive(result.get("observation"))
    if error == "expectation_timeout" and unreadable:
        return SKIPPED, public_reason("screen not readable", error)
    # A batch that ran out of time before executing anything never started.
    if error in ("batch_deadline", "batch_deadline_after_action") and _zero(
        result.get("completed")
    ) and _zero(result.get("applied_actions")):
        return SKIPPED, public_reason("did not start", error)
    # A failure we recognise as the flow's own.
    if error in KNOWN_FLOW_ERRORS:
        return FAILED, public_reason("step failed", error, step)
    if ok is False and error is None and isinstance(step, int) and not isinstance(step, bool):
        return FAILED, public_reason("step failed", None, step)
    # Something went wrong that we do not understand. Saying the flow is broken
    # would be a guess, and guesses here tag flows out of the registry.
    return SKIPPED, public_reason("no verdict in the result")


def _zero(value):
    return isinstance(value, int) and not isinstance(value, bool) and value == 0


def observation_is_inconclusive(observation):
    """True when the daemon's own observation says the screen was not seen.

    Mirrors the evidence `/agent/actions` reports: `read:false` (no tree was
    ever obtained), `stale:true` (the last read failed, so this is an older
    look), and `sparse` with unproven `absent` locators (a tree too bare to
    prove anything is gone).
    """
    if not isinstance(observation, dict):
        # No observation at all is not evidence the flow is fine — but it is
        # also not evidence it is broken.
        return True
    if observation.get("read") is not True:
        return True
    if observation.get("stale") is True:
        return True
    unproven = observation.get("absent_unproven")
    if observation.get("sparse") is True and isinstance(unproven, list) and unproven:
        return True
    return False


def run_flow(f, dry, artifacts_dir=None):
    """Always returns (verdict, public_reason, result-dict). Never raises."""
    args = ["flow", "run", f["id"]]
    for k, v in (f.get("example_inputs") or {}).items():
        args += ["--input", f"{k}={v}"]
    if artifacts_dir:
        args += ["--artifacts-dir", artifacts_dir]
    if dry:
        return VERIFIED, "", {"dry_run": True}
    try:
        rc, out, err = mcp(*args, timeout=240)
    except subprocess.TimeoutExpired:
        # The child never finished. We do not know what the phone did, so this
        # is not a verdict about the flow.
        return SKIPPED, public_reason("unknown outcome"), {"error": "outcome_unknown"}
    except Exception:
        return SKIPPED, public_reason("no machine-readable result"), {}
    # `flow run` prints the complete machine-readable result on stdout even
    # when it exits non-zero, so the result is read from stdout — never
    # scraped out of an error message.
    result = None
    if out:
        try:
            parsed = json.loads(out)
            if isinstance(parsed, dict):
                result = parsed
        except Exception:
            result = None
    if result is None:
        # Keep the tail for the local log only; it never reaches a public body.
        result = {"raw": (out or err)[-800:]}
    verdict, reason = classify(rc, result)
    return verdict, reason, result


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run"); r.add_argument("--dry-run", action="store_true"); r.add_argument("--only", default=""); r.add_argument("--device", default="")
    sub.add_parser("enable"); sub.add_parser("disable"); sub.add_parser("status")
    a = ap.parse_args()
    if a.cmd in ("enable", "disable", "status"):
        return launchd(a.cmd)
    return run(a)


def run(a):
    if not TOKEN:
        log("PHONE_REMOTE_TOKEN not set; nothing to do"); return 0
    if not os.path.exists(MCP):
        log(f"iphone-use-mcp not found at {MCP}"); return 0
    st, why = preflight()
    if why:
        log(f"skip: {why}"); return 0
    if a.dry_run and not st.get("drivable"):
        log("dry run: phone not drivable; would request a bring-up"); return 0
    if not bring_up(st):
        log("skip: phone not drivable"); return 0
    apps = installed_versions()
    device = a.device or (apps or {}).get("device") or "unknown device"
    ios = (apps or {}).get("ios")
    only = set(x for x in a.only.split(",") if x)
    flows = canary_flows(only)
    log(f"reverify start · {len(flows)} canary flow(s) · {device} · iOS {ios}")
    artifacts_dir = os.path.join(LOG_DIR, "canary", datetime.date.today().isoformat())
    results = []
    try:
        for f in flows:
            verdict, reason, result = run_flow(f, a.dry_run, artifacts_dir)
            results.append((f, verdict, reason, result))
            detail = json.dumps(
                {k: result.get(k) for k in ("completed", "failed_step", "error", "artifact")},
                ensure_ascii=False,
            )
            log(f"  {verdict.upper()} {f['id']} · {reason or ''} · {detail}")
            # go home between flows so each starts from a known state
            if not a.dry_run:
                try:
                    mcp("flow", "run", "system/home", timeout=60)
                except Exception as e:
                    log(f"  (could not return home: {e})")
    finally:
        # Hand the phone back even if the loop died. Holding a lease we are no
        # longer using locks everyone else out for its full duration.
        #
        # A dry run never took the lease and never sent anything, so it must
        # not send this either: a rehearsal that mutates state is not a
        # rehearsal.
        if not a.dry_run:
            try:
                http("POST", "/agent/owner", {"release": True}, control=True, timeout=10)
            except Exception as e:
                log(f"  (could not release the owner lease: {e})")
    verified_count = sum(1 for _, verdict, _, _ in results if verdict == VERIFIED)
    failed_count = sum(1 for _, verdict, _, _ in results if verdict == FAILED)
    skipped_count = sum(1 for _, verdict, _, _ in results if verdict == SKIPPED)
    log(
        f"reverify done · {verified_count} verified · {failed_count} failed · "
        f"{skipped_count} skipped · evidence in {artifacts_dir}"
    )
    if a.dry_run or not flows:
        log("dry run / nothing to record"); return 0
    if not verified_count and not failed_count:
        log("nothing was judged tonight; leaving the registry untouched"); return 0
    return record(results, device, ios, apps)


def record(results, device, ios, apps):
    if not shutil.which("gh"):
        log("gh not installed; results not recorded"); return 0
    today = datetime.date.today().isoformat()
    work = tempfile.mkdtemp(prefix="flow-reverify-")
    repo = os.path.join(work, "repo")
    sh(["gh", "repo", "clone", REPO, repo, "--", "--depth", "1", "--quiet"])
    branch = f"reverify/{today}"
    sh(["git", "checkout", "-q", "-b", branch], cwd=repo)
    changed, failed = [], []
    skipped = [(f, reason) for f, verdict, reason, _ in results if verdict == SKIPPED]
    for f, verdict, reason, result in results:
        # A skipped flow is left exactly as it was: no refreshed date, no tag,
        # no issue. The night simply did not answer the question.
        if verdict == SKIPPED:
            continue
        path = os.path.join(repo, f["id"] + ".json")
        if not os.path.exists(path):
            continue
        ok = verdict == VERIFIED
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
        tags = doc.get("tags", [])
        if ok:
            ver = compat_version(apps, doc.get("app"))
            entry = {"device": device, "ios": ios, "app_version": ver, "date": today}
            entry = {k: v for k, v in entry.items() if v}
            others = [v for v in doc.get("verified_on", []) if v.get("device") != device]
            doc["verified_on"] = (others + [entry])[-16:]
            if "needs-verification" in tags:
                tags.remove("needs-verification")
        else:
            if "needs-verification" not in tags:
                tags.append("needs-verification")
            failed.append((f, result))
        doc["tags"] = tags if tags else doc.get("tags", [])
        if not tags and "tags" in doc:
            del doc["tags"]
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(doc, fh, ensure_ascii=False, indent=2); fh.write("\n")
        changed.append(f["id"])
    if not changed:
        log("no registry changes"); return 0
    sh(["python3", "scripts/build-index.py"], cwd=repo)
    sh(["git", "add", "-A"], cwd=repo)
    verified_count = sum(1 for _, verdict, _, _ in results if verdict == VERIFIED)
    title = (
        f"reverify {today}: {verified_count} verified, {len(failed)} failed, "
        f"{len(skipped)} skipped on {device}"
    )
    sh(["git", "-c", "user.name=flow-reverify", "-c", "user.email=flow-reverify@users.noreply.github.com", "commit", "-q", "-m", title], cwd=repo)
    sh(["git", "push", "-q", "-u", "origin", branch], cwd=repo)
    body = [f"Nightly canary run on **{device}** (iOS {ios}) — {today}.", "", "| flow | verdict | detail |", "|---|---|---|"]
    for f, verdict, reason, result in results:
        if verdict == VERIFIED:
            body.append(f"| `{f['id']}` | ✅ verified | `verified_on` refreshed |")
        elif verdict == FAILED:
            body.append(
                f"| `{f['id']}` | ❌ failed | {reason} → tagged `needs-verification` |"
            )
        else:
            body.append(f"| `{f['id']}` | ⏭️ skipped | {reason}; left untouched |")
    if skipped:
        body += [
            "",
            "Skipped flows were **not** judged: the phone was not in a state to answer the "
            "question, so no date was refreshed and no flow was tagged. They are re-tried "
            "on the next run.",
        ]
    body += ["", "_Opened by `scripts/flow-reverify.py`. Merge to publish the refreshed `verified_on` dates; fix failing flows in a follow-up PR._"]
    pr = sh(["gh", "pr", "create", "-R", REPO, "--head", branch, "--title", title, "--body", "\n".join(body)])
    log(f"PR: {pr}")
    for f, result in failed:
        rc, out, err = mcp("flow", "report", f["id"], "--result", json.dumps(result), "--note", f"Nightly re-verification failed on {device} (iOS {ios}, {today}). Flow tagged needs-verification in {pr}.")
        log(f"  report {f['id']}: {out or err}")
    return 0


def launchd(cmd):
    plist = os.path.expanduser(f"~/Library/LaunchAgents/{LABEL}.plist")
    uid = os.getuid()
    if cmd == "status":
        print("installed" if os.path.exists(plist) else "not installed", plist)
        subprocess.run(["launchctl", "print", f"gui/{uid}/{LABEL}"], capture_output=False)
        return 0
    if cmd == "disable":
        subprocess.run(["launchctl", "bootout", f"gui/{uid}/{LABEL}"], capture_output=True)
        if os.path.exists(plist):
            os.remove(plist)
        print("disabled"); return 0
    env = {k: os.environ[k] for k in ("PHONE_REMOTE_URL", "PHONE_REMOTE_TOKEN", "IPHONE_USE_MCP", "IPHONE_USE_FLOWS_REPO", "FLOW_REVERIFY_OWNER") if os.environ.get(k)}
    env.setdefault("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    if "PHONE_REMOTE_TOKEN" not in env:
        print("set PHONE_REMOTE_TOKEN (and PHONE_REMOTE_URL) in the environment when enabling", file=sys.stderr); return 2
    data = {"Label": LABEL, "ProgramArguments": ["/usr/bin/python3", os.path.abspath(__file__), "run"],
            "StartCalendarInterval": {"Hour": 3, "Minute": 30}, "EnvironmentVariables": env,
            "StandardOutPath": LOG, "StandardErrorPath": LOG, "RunAtLoad": False}
    os.makedirs(os.path.dirname(plist), exist_ok=True); os.makedirs(LOG_DIR, exist_ok=True)
    with open(plist, "wb") as fh:
        plistlib.dump(data, fh)
    subprocess.run(["launchctl", "bootout", f"gui/{uid}/{LABEL}"], capture_output=True)
    sh(["launchctl", "bootstrap", f"gui/{uid}", plist])
    print(f"enabled: daily 03:30 → {LOG}"); return 0


if __name__ == "__main__":
    sys.exit(main())
