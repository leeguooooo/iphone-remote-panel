#!/usr/bin/env python3
"""Run the real setup status helpers in isolated homes, without a device."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()
HELPERS = SOURCE.split('# BEGIN setup status protocol', 1)[1].split('# END setup status protocol.', 1)[0]
HELPERS = HELPERS[HELPERS.index('\n') + 1:]


class StatusTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='wda-status-test-')
        self.root = Path(self.temporary.name)
        self.state = self.root / 'state'
        self.state.mkdir()
        self.path = self.state / 'wda-setup-status.json'
        self.helpers = self.root / 'helpers.sh'
        self.helpers.write_text(HELPERS)
        self.processes = []

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            if process.stderr:
                process.stderr.close()
        self.temporary.cleanup()

    def start(self, body, *, finish=True):
        script = self.root / ('run-' + str(len(self.processes)) + '.sh')
        script.write_text(
            'set -eu\nCOMMAND=setup\nSTATUS_RUN_ID=""\n'
            'STATUS_FILE="$STATUS_TEST_DIR/wda-setup-status.json"\n'
            '. "$STATUS_TEST_HELPERS"\n'
            + ('trap \'_status_finish_run "$?"\' EXIT\ntrap \'exit 130\' INT TERM\n' if finish else '')
            + '_status_begin_run\n' + body + '\n'
        )
        process = subprocess.Popen(
            ['/bin/bash', str(script)], start_new_session=True,
            env={**os.environ, 'STATUS_TEST_DIR': str(self.state), 'STATUS_TEST_HELPERS': str(self.helpers)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        self.processes.append(process)
        return process

    def state_when(self, predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = json.loads(self.path.read_text())
                if predicate(value):
                    return value
            except (FileNotFoundError, ValueError):
                pass
            time.sleep(0.05)
        self.fail('status did not reach expected state')

    def done(self, process, expected=0):
        # Long enough for the heartbeat test's 27s body to finish after the
        # beat was observed; only a hung helper ever waits this out.
        _, errors = process.communicate(timeout=20)
        self.assertEqual(process.returncode, expected, errors)

    def test_heartbeat_during_synchronous_work_and_owner_identity(self):
        # The heartbeat fires every 15s. Waiting 16s left one second for the
        # helper's python to start on a loaded CI runner, which is why this
        # test failed on roughly every other release run; give it a real margin.
        process = self.start('_setstatus building "" "long prebuild"\nsleep 27\n_setstatus ready "" "ready"')
        initial = self.state_when(lambda s: s['phase'] == 'building')
        self.assertEqual(initial['owner_pid'], process.pid)
        self.assertTrue(initial['owner_start'])
        beat = self.state_when(lambda s: s['heartbeat_ts'] > initial['heartbeat_ts'], timeout=25)
        self.assertEqual(beat['phase'], 'building')
        self.assertEqual(beat['phase_started_at'], initial['phase_started_at'])
        self.done(process)
        result = json.loads(self.path.read_text())
        self.assertEqual(result['phase'], 'ready')
        self.assertFalse(result['active'])
        self.assertTrue(result['terminal'])
        self.assertEqual(result['exit_code'], 0)

    def test_failure_preserves_blocker_and_diagnostic(self):
        process = self.start('_setstatus building account \'missing "profile"\'\nexit 7')
        self.done(process, 7)
        result = json.loads(self.path.read_text())
        self.assertEqual(result['phase'], 'building-fail')
        self.assertEqual(result['last_phase'], 'building')
        self.assertEqual(result['message'], 'missing "profile"')
        self.assertEqual(result['blocked_on'], 'account')
        self.assertFalse(result['active'])
        self.assertEqual(result['exit_code'], 7)

    def test_stop_signal_writes_terminal_state(self):
        process = self.start('_setstatus ddi-wait usb "waiting"\nwhile :; do sleep 0.1; done')
        self.state_when(lambda s: s['phase'] == 'ddi-wait')
        process.terminate()
        self.done(process, 130)
        result = json.loads(self.path.read_text())
        self.assertEqual(result['phase'], 'stopped')
        self.assertEqual(result['blocked_on'], 'usb')
        self.assertTrue(result['terminal'])

    def test_old_updates_and_exit_cannot_overwrite_new_owner(self):
        old = self.start('_setstatus building "" "old"\nwhile [ ! -f "$STATUS_TEST_DIR/release-old" ]; do sleep 0.1; done\n_status_publish heartbeat\n_setstatus ready "" "old done"')
        first = self.state_when(lambda s: s['message'] == 'old')
        new = self.start('_setstatus building "" "new"\nwhile :; do sleep 0.1; done')
        current = self.state_when(lambda s: s['message'] == 'new')
        self.assertNotEqual(first['run_id'], current['run_id'])
        (self.state / 'release-old').touch()
        self.done(old)
        after = json.loads(self.path.read_text())
        self.assertEqual(after['run_id'], current['run_id'])
        self.assertEqual(after['phase'], 'building')
        self.assertEqual(after['message'], 'new')
        new.terminate()
        self.done(new, 130)

    def test_parent_death_cannot_leave_live_heartbeat(self):
        process = self.start('_setstatus building "" "running"\nwhile :; do sleep 0.1; done', finish=False)
        state = self.state_when(lambda s: s['phase'] == 'building')
        # Kill only the fixture parent: the watcher is allowed to observe death.
        process.kill()
        process.wait(timeout=5)
        after = self.state_when(lambda s: s.get('phase') == 'interrupted', timeout=4)
        self.assertEqual(after['run_id'], state['run_id'])
        self.assertFalse(after['active'])
        self.assertTrue(after['terminal'])

    def test_existing_terminal_failure_is_not_erased(self):
        process = self.start('_setstatus signing-fail account "sign in"\nexit 1')
        self.done(process, 1)
        self.assertEqual(json.loads(self.path.read_text())['phase'], 'signing-fail')

    def test_status_symlink_is_not_followed(self):
        target = self.root / 'unrelated'
        target.write_text('leave alone')
        self.path.symlink_to(target)
        process = self.start(':')
        self.done(process, 1)
        self.assertEqual(target.read_text(), 'leave alone')


if __name__ == '__main__':
    unittest.main()
