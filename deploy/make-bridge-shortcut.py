#!/usr/bin/env python3
"""Generate the "iU Bridge" Shortcuts file for the semantic intents channel.

The daemon dispatches a verb by opening
``shortcuts://run-shortcut?name=<bridge>&input=text&text=<{verb,id,args} JSON>``
on the phone through WDA. This script builds the other half: the shortcut that
parses that request, runs the native action for the verb, and POSTs
``{id, verb, ok, data}`` back to ``/agent/inbox`` so the daemon can match it to
the dispatch id.

Usage (see --help):

    python3 deploy/make-bridge-shortcut.py --token "$PHONE_REMOTE_AGENT_TOKEN"
    open "iU Bridge.shortcut"        # accept the import dialog on the Mac

The imported shortcut's name is the file stem, and it MUST equal the registry's
``bridge.name`` (``~/.iphone-use/intents-registry.json``) or dispatch will not
find it. iCloud sync carries it to the phone; run each verb once by hand there
to clear its interactive permission prompt.

Two lessons from the hardware validation of #55 are baked in, and both are
easy to undo by accident:

* **Parse with regex, not dictionary actions.** The obvious
  ``detect.dictionary`` + ``getvalueforkey`` chain silently yields *empty*
  values here — no error, just nothing. ``text.match`` + ``getgroup`` works.
* **The bearer token lives only inside the shortcut's stored headers.** It must
  never travel in the deep link, which is why dispatch carries no credential.

The daemon must be reachable *from the phone* for the return POST; a
loopback-only daemon can dispatch and never hear back (issue #59). Hence
``--daemon-url`` defaults to this Mac's ``.local`` name rather than localhost.
"""

from __future__ import annotations

import argparse
import json
import plistlib
import subprocess
import sys
import uuid
from pathlib import Path

BRIDGE_VERSION = 3

# Shortcuts renders an attached variable as this single object-replacement
# character, and the attachment map is keyed by its {offset, length} in the
# string — so every offset below is computed from the literal text, never
# hardcoded.
OBJ = "￼"


def _uuid() -> str:
    return str(uuid.uuid4()).upper()


def _token(text: str, attachments: dict) -> dict:
    return {
        "Value": {"string": text, "attachmentsByRange": attachments},
        "WFSerializationType": "WFTextTokenString",
    }


def _output(output_uuid: str, name: str) -> dict:
    return {
        "Value": {"Type": "ActionOutput", "OutputUUID": output_uuid, "OutputName": name},
        "WFSerializationType": "WFTextTokenAttachment",
    }


def _match(action_uuid: str, pattern: str) -> dict:
    """Regex-match against Shortcut Input (the dispatched JSON request)."""
    return {
        "WFWorkflowActionIdentifier": "is.workflow.actions.text.match",
        "WFWorkflowActionParameters": {
            "UUID": action_uuid,
            "text": _token(OBJ, {"{0, 1}": {"Type": "ExtensionInput"}}),
            "WFMatchTextPattern": pattern,
        },
    }


def _group(action_uuid: str, match_uuid: str) -> dict:
    """First capture group of a preceding text.match."""
    return {
        "WFWorkflowActionIdentifier": "is.workflow.actions.text.match.getgroup",
        "WFWorkflowActionParameters": {
            "UUID": action_uuid,
            "matches": _output(match_uuid, "Matches"),
            "WFGetGroupType": "Group At Index",
            "WFGroupIndex": 1,
        },
    }


def _json_field(pattern_key: str) -> str:
    return r'"%s"\s*:\s*"([^"]*)"' % pattern_key


def _response_text(id_uuid: str, verb: str, data_parts: list[tuple[str, str, str]]) -> dict:
    """Build the `{id, verb, ok, data}` response body as a text token.

    `data_parts` is a list of (json_key, output_uuid, output_name); each becomes
    a numeric field whose value is that action's output.
    """
    head = '{"id":"'
    mid = '","verb":"%s","ok":true,"data":{"bridge_version":%d' % (verb, BRIDGE_VERSION)
    text = head + OBJ + mid
    attachments = {"{%d, 1}" % len(head): {
        "Type": "ActionOutput", "OutputUUID": id_uuid, "OutputName": "Match Group"}}
    for key, out_uuid, out_name in data_parts:
        prefix = ',"%s":' % key
        text += prefix
        attachments["{%d, 1}" % len(text)] = {
            "Type": "ActionOutput", "OutputUUID": out_uuid, "OutputName": out_name}
        text += OBJ
    text += "}}"
    return _token(text, attachments)


def _post_action(text_uuid: str, daemon_url: str, token: str) -> dict:
    header = lambda k, v: {"WFItemType": 0, "WFKey": _token(k, {}), "WFValue": _token(v, {})}
    return {
        "WFWorkflowActionIdentifier": "is.workflow.actions.downloadurl",
        "WFWorkflowActionParameters": {
            "WFURL": daemon_url.rstrip("/") + "/agent/inbox",
            "WFHTTPMethod": "POST",
            "ShowHeaders": True,
            "WFHTTPHeaders": {
                "Value": {"WFDictionaryFieldValueItems": [
                    header("Authorization", "Bearer " + token),
                    header("X-Phone-Control", "1"),
                    header("Content-Type", "application/json"),
                ]},
                "WFSerializationType": "WFDictionaryFieldValue",
            },
            "WFHTTPBodyType": "File",
            "WFRequestVariable": _output(text_uuid, "Text"),
        },
    }


def _conditional(group_id: str, mode: int, verb_output: str | None = None,
                 verb: str | None = None) -> dict:
    params: dict = {"GroupingIdentifier": group_id, "WFControlFlowMode": mode}
    if mode == 0:
        params["WFInput"] = _output(verb_output, "Match Group")
        params["WFCondition"] = "Equals"
        params["WFConditionalActionString"] = verb
    return {
        "WFWorkflowActionIdentifier": "is.workflow.actions.conditional",
        "WFWorkflowActionParameters": params,
    }


# Native action per verb. Each entry returns (actions, data_parts) given a
# fresh UUID; `data_parts` are folded into the response `data` object.
def _verb_ping(_uuid_fn):
    return [], []


def _verb_battery(uuid_fn):
    battery = uuid_fn()
    return (
        [{"WFWorkflowActionIdentifier": "is.workflow.actions.getbatterylevel",
          "WFWorkflowActionParameters": {"UUID": battery}}],
        [("level", battery, "Battery Level")],
    )


VERBS = {"ping": _verb_ping, "battery": _verb_battery}


def build(verbs: list[str], daemon_url: str, token: str) -> dict:
    unknown = [v for v in verbs if v not in VERBS]
    if unknown:
        raise SystemExit(
            "unknown verb(s): %s\nKnown: %s\nAdd a builder to VERBS (and the registry entry) "
            "for a new verb — the bridge is a reviewed capability list, not a runtime escape "
            "hatch." % (", ".join(unknown), ", ".join(sorted(VERBS)))
        )

    match_id, group_id_action = _uuid(), _uuid()
    match_verb, group_verb = _uuid(), _uuid()
    actions = [
        _match(match_id, _json_field("id")), _group(group_id_action, match_id),
        _match(match_verb, _json_field("verb")), _group(group_verb, match_verb),
    ]

    # One self-contained branch per verb: its native action, its own response
    # text, its own POST. Duplicating the POST costs a few plist nodes and
    # avoids plumbing a variable across control-flow boundaries, which is
    # where hand-built bridges tend to silently lose their value.
    for verb in verbs:
        branch = _uuid()
        actions.append(_conditional(branch, 0, group_verb, verb))
        verb_actions, data_parts = VERBS[verb](_uuid)
        actions.extend(verb_actions)
        text_uuid = _uuid()
        actions.append({
            "WFWorkflowActionIdentifier": "is.workflow.actions.gettext",
            "WFWorkflowActionParameters": {
                "UUID": text_uuid,
                "WFTextActionText": _response_text(group_id_action, verb, data_parts),
            },
        })
        actions.append(_post_action(text_uuid, daemon_url, token))
        actions.append(_conditional(branch, 2))

    return {
        "WFWorkflowMinimumClientVersion": 900,
        "WFWorkflowMinimumClientVersionString": "900",
        "WFWorkflowClientVersion": "2607.0.3",
        "WFWorkflowHasShortcutInputVariables": True,
        "WFWorkflowIcon": {
            "WFWorkflowIconStartColor": 431817727,
            "WFWorkflowIconGlyphNumber": 59511,
        },
        "WFWorkflowInputContentItemClasses": ["WFStringContentItem"],
        "WFWorkflowImportQuestions": [],
        "WFWorkflowTypes": [],
        "WFQuickActionSurfaces": [],
        "WFWorkflowActions": actions,
    }


def default_daemon_url() -> str:
    try:
        host = subprocess.run(["scutil", "--get", "LocalHostName"],
                              capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        host = "localhost"
    return "http://%s.local:45432" % host


def self_test() -> int:
    """Check the two things that fail *silently* in a Shortcuts plist.

    A variable is attached by its ``{offset, length}`` into the literal string,
    so an off-by-one lands the value in the wrong place — or nowhere — and the
    shortcut still imports and runs, just POSTing a broken body. And an
    unbalanced conditional group makes Shortcuts drop the branch. Neither shows
    up as an error anywhere, which is exactly why they are pinned here.
    """
    workflow = build(sorted(VERBS), "http://mac.local:45432", "tok")
    problems = []
    for action in workflow["WFWorkflowActions"]:
        params = action["WFWorkflowActionParameters"]
        text = params.get("WFTextActionText")
        if not text:
            continue
        string, attachments = text["Value"]["string"], text["Value"]["attachmentsByRange"]
        for key in attachments:
            offset = int(key.strip("{}").split(",")[0])
            if offset >= len(string) or string[offset] != OBJ:
                problems.append("attachment at %d does not land on a variable slot: %r"
                                % (offset, string))
        for index, char in enumerate(string):
            if char == OBJ and "{%d, 1}" % index not in attachments:
                problems.append("variable slot at %d has no attachment: %r" % (index, string))
        # Substitute a bare `0`, not a quoted token: a slot may sit inside
        # quotes (`"id":"<var>"`) or outside them (`"level":<var>`), and 0 is
        # the one literal that stays valid JSON in both positions.
        body = string.replace(OBJ, "0")
        try:
            json.loads(body)
        except ValueError as error:
            problems.append("response body is not valid JSON (%s): %r" % (error, body))

    groups: dict[str, list[int]] = {}
    for action in workflow["WFWorkflowActions"]:
        if action["WFWorkflowActionIdentifier"] == "is.workflow.actions.conditional":
            params = action["WFWorkflowActionParameters"]
            groups.setdefault(params["GroupingIdentifier"], []).append(params["WFControlFlowMode"])
    for group, modes in groups.items():
        if modes != [0, 2]:
            problems.append("conditional group %s is unbalanced: %s" % (group[:8], modes))

    for problem in problems:
        print("FAIL: %s" % problem, file=sys.stderr)
    print("self-test: %s (%d verbs, %d actions, %d branches)"
          % ("PASS" if not problems else "FAIL", len(VERBS),
             len(workflow["WFWorkflowActions"]), len(groups)))
    return 1 if problems else 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true",
                        help="verify variable slots, response JSON, and branch balance; "
                             "no files written, no token needed")
    parser.add_argument("--token", required="--self-test" not in sys.argv,
                        help="daemon bearer token (PHONE_REMOTE_AGENT_TOKEN); stored inside "
                             "the shortcut's headers, never in the deep link")
    parser.add_argument("--daemon-url", default=None,
                        help="daemon base URL reachable FROM THE PHONE "
                             "(default: this Mac's .local name, port 45432)")
    parser.add_argument("--name", default="iU Bridge",
                        help="shortcut name; must equal the registry's bridge.name")
    parser.add_argument("--verb", action="append", dest="verbs",
                        help="verb to include (repeatable; default: ping battery)")
    parser.add_argument("--out-dir", default=".", type=Path)
    parser.add_argument("--no-sign", action="store_true",
                        help="write the unsigned plist only (skip `shortcuts sign`)")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    verbs = args.verbs or ["ping", "battery"]
    daemon_url = args.daemon_url or default_daemon_url()
    workflow = build(verbs, daemon_url, args.token)

    args.out_dir.mkdir(parents=True, exist_ok=True)
    unsigned = args.out_dir / ("%s.unsigned.shortcut" % args.name)
    unsigned.write_bytes(plistlib.dumps(workflow))

    if args.no_sign:
        print("wrote %s (unsigned)" % unsigned)
        return 0

    signed = args.out_dir / ("%s.shortcut" % args.name)
    result = subprocess.run(
        ["shortcuts", "sign", "--mode", "anyone", "-i", str(unsigned), "-o", str(signed)],
        capture_output=True, text=True)
    if result.returncode != 0:
        print("shortcuts sign failed:\n%s%s" % (result.stdout, result.stderr), file=sys.stderr)
        return 1
    unsigned.unlink()
    print("wrote %s\n  verbs   : %s\n  posts to: %s/agent/inbox" %
          (signed, " ".join(verbs), daemon_url.rstrip("/")))
    print("\nNext: `open %s` and accept the import dialog, then run each verb once on the "
          "phone to clear its permission prompt." % json.dumps(str(signed)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
