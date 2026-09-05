#!/usr/bin/env python3
"""Run the post-handoff product verdict from setup-wda.sh against canned
`/agent/status` bodies — no device, no daemon, no curl."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()


def function(name):
    return '\n' + name + '() {' + SOURCE.split('\n' + name + '() {', 1)[1].split('\n}\n', 1)[0] + '\n}\n'


HELPERS = ''.join(function(name) for name in ('_daemon_product_verdict', '_daemon_status_reports_locked'))

GATE_START = '    DAEMON_STATUS_TRIES=0\n    DAEMON_PRODUCT_VERDICT=down\n'
GATE = SOURCE[SOURCE.index(GATE_START):]
GATE = GATE[:GATE.index('\nif [ "$WDA_MARKER_REFRESH_ALLOWED" = "1" ]; then')]
GATE = 'if [ "$DAEMON_HTTP_READY" = "1" ]; then\n' + GATE + '\n'

# A real v0.6.2 body, trimmed to the fields the verdict reads plus decoys.
BASE = {
    "ok": True, "backend": "direct", "managed_wda": True, "managed_wda_pending": False,
    "wda": False, "wda_actionable": False, "wda_locked": None, "drivable": False,
    "reconnecting": True, "released": True, "mode": "offline",
}


def status(**overrides):
    body = dict(BASE)
    body.update(overrides)
    return json.dumps(body)


class VerdictTests(unittest.TestCase):
    def run_verdict(self, body):
        script = HELPERS + '\n_daemon_product_verdict "$1"\n' \
            'if _daemon_status_reports_locked "$1"; then echo locked; else echo not-locked; fi\n'
        out = subprocess.run(['bash', '-c', script, 'x', body], capture_output=True, text=True, check=True)
        return out.stdout.split()

    def test_reachable_but_not_actionable_is_reachable(self):
        self.assertEqual(self.run_verdict(status(wda=True)), ['reachable', 'not-locked'])

    def test_locked_phone_is_reachable_and_flagged(self):
        self.assertEqual(self.run_verdict(status(wda=True, wda_locked=True)), ['reachable', 'locked'])

    def test_drivable_wins(self):
        self.assertEqual(
            self.run_verdict(status(wda=True, wda_actionable=True, drivable=True, reconnecting=False, released=False)),
            ['drivable', 'not-locked'])

    def test_decoy_keys_never_count_as_reachable(self):
        # managed_wda:true and wda_actionable:true must not satisfy the `"wda"` match.
        self.assertEqual(self.run_verdict(status(managed_wda=True, wda_actionable=True)), ['down', 'not-locked'])

    def test_empty_or_garbage_is_down(self):
        self.assertEqual(self.run_verdict(''), ['down', 'not-locked'])
        self.assertEqual(self.run_verdict('<html>Bad request.</html>'), ['down', 'not-locked'])

    def test_locked_false_or_null_is_not_locked(self):
        self.assertEqual(self.run_verdict(status(wda=True, wda_locked=False))[1], 'not-locked')
        self.assertEqual(self.run_verdict(status(wda=True, wda_locked=None))[1], 'not-locked')


class GateTests(unittest.TestCase):
    """Drive the real gate block with a scripted curl and no sleeping."""

    def run_gate(self, bodies, *, max_tries=6, grace=3):
        with tempfile.TemporaryDirectory(prefix='wda-verdict-test-') as tmp:
            root = Path(tmp)
            (root / 'responses').write_text('\n'.join(bodies) + '\n')
            script = root / 'gate.sh'
            script.write_text(
                'set -u\n'
                + HELPERS +
                'RESPONSES="$1"\nLOG="$2"\n'
                # `curl` runs inside $(...), so count calls in a file, not a shell variable.
                'curl() { n=$(( $(cat "$RESPONSES.n" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$RESPONSES.n"; sed -n "${n}p" "$RESPONSES"; }\n'
                'sleep() { :; }\n'
                'ok() { printf "ok: %s\\n" "$*" >> "$LOG"; }\n'
                'warn() { printf "warn: %s\\n" "$*" >> "$LOG"; }\n'
                'die() { printf "die: %s\\n" "$*" >> "$LOG"; exit 1; }\n'
                '_setstatus() { printf "status: %s\\n" "$*" >> "$LOG"; }\n'
                'DAEMON_HTTP_READY=1\nDAEMON_PRODUCT_READY=0\nDAEMON_PORT=1\nDAEMON_AGENT_SECRET=secret\n'
                f'DAEMON_STATUS_MAX_TRIES={max_tries}\nDAEMON_REACHABLE_GRACE_TRIES={grace}\n'
                + GATE +
                'printf "tries=%s verdict=%s ready=%s\\n" "$DAEMON_STATUS_TRIES" "$DAEMON_PRODUCT_VERDICT" "$DAEMON_PRODUCT_READY" >> "$LOG"\n'
            )
            log = root / 'log'
            log.write_text('')
            proc = subprocess.run(['bash', str(script), str(root / 'responses'), str(log)],
                                  capture_output=True, text=True)
            return proc.returncode, log.read_text().splitlines()

    def test_locked_phone_passes_after_grace_with_a_hint(self):
        code, log = self.run_gate([status(), status(wda=True, wda_locked=True)] + [status(wda=True, wda_locked=True)] * 10)
        self.assertEqual(code, 0, log)
        self.assertIn('ok: daemon product status verified: WDA reachable through the relays', log)
        self.assertTrue(any(line.startswith('warn: the iPhone is locked') for line in log), log)
        self.assertIn('tries=4 verdict=reachable ready=1', log)  # 1 down + 3 reachable (grace)
        self.assertFalse(any(line.startswith('die:') for line in log), log)

    def test_drivable_inside_grace_short_circuits(self):
        code, log = self.run_gate([status(wda=True), status(wda=True, drivable=True, reconnecting=False, released=False)])
        self.assertEqual(code, 0, log)
        self.assertIn('ok: daemon product status verified: drivable=true', log)
        self.assertIn('tries=2 verdict=drivable ready=1', log)
        self.assertFalse(any(line.startswith('warn:') for line in log), log)

    def test_unreachable_wda_still_fails_closed(self):
        code, log = self.run_gate([status()] * 6)
        self.assertEqual(code, 1, log)
        self.assertIn('status: daemon-fail wda daemon never reached WDA after verified WDA handoff', log)
        self.assertTrue(any(line.startswith('die:') and 'wda=true within 3s' in line for line in log), log)

    def test_reachable_unlocked_but_not_actionable_warns_without_locked_hint(self):
        code, log = self.run_gate([status(wda=True, wda_locked=False)] * 6)
        self.assertEqual(code, 0, log)
        self.assertTrue(any('cannot act yet' in line for line in log), log)
        self.assertFalse(any('is locked' in line for line in log), log)


if __name__ == '__main__':
    unittest.main()
