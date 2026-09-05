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


def run_flow(f, dry):
    args = ["flow", "run", f["id"]]
    for k, v in (f.get("example_inputs") or {}).items():
        args += ["--input", f"{k}={v}"]
    if dry:
        return True, {"dry_run": True}
    rc, out, err = mcp(*args, timeout=240)
    text = out or err
    try:
        start = text.find("{")
        result = json.loads(text[start:]) if start >= 0 else {"raw": text}
    except Exception:
        result = {"raw": text[-800:]}
    return rc == 0 and result.get("ok") is True, result


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
    results = []
    for f in flows:
        ok, result = run_flow(f, a.dry_run)
        results.append((f, ok, result))
        log(f"  {'PASS' if ok else 'FAIL'} {f['id']} · {json.dumps({k: result.get(k) for k in ('completed','failed_step','error')}, ensure_ascii=False)}")
        # go home between flows so each starts from a known state
        if not a.dry_run:
            mcp("flow", "run", "system/home", timeout=60)
    http("POST", "/agent/owner", {"release": True}, control=True, timeout=10)
    if a.dry_run or not flows:
        log("dry run / nothing to record"); return 0
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
    for f, ok, result in results:
        path = os.path.join(repo, f["id"] + ".json")
        if not os.path.exists(path):
            continue
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
    title = f"reverify {today}: {sum(1 for _, ok, _ in results if ok)} pass, {len(failed)} fail on {device}"
    sh(["git", "-c", "user.name=flow-reverify", "-c", "user.email=flow-reverify@users.noreply.github.com", "commit", "-q", "-m", title], cwd=repo)
    sh(["git", "push", "-q", "-u", "origin", branch], cwd=repo)
    body = [f"Nightly canary run on **{device}** (iOS {ios}) — {today}.", "", "| flow | result |", "|---|---|"]
    for f, ok, result in results:
        body.append(f"| `{f['id']}` | {'✅ verified_on refreshed' if ok else '❌ failed at step ' + str(result.get('failed_step')) + ' (' + str(result.get('error')) + ') → tagged needs-verification'} |")
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
