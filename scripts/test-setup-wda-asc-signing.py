#!/usr/bin/env python3
"""Exercise real signing/identity helpers with fake values; never call Xcode."""
import itertools
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()
ASC_NAMES = ('WDA_ASC_KEY_PATH', 'WDA_ASC_KEY_ID', 'WDA_ASC_ISSUER_ID')
FAKE_ASC = dict(zip(ASC_NAMES, (
    "/nonexistent/fixture keys/AuthKey_FAKE123456.p8",
    'FAKE123456', '00000000-1111-2222-3333-444444444444',
)))


def function(name):
    return '\n' + name + '() {' + SOURCE.split('\n' + name + '() {', 1)[1].split('\n}\n', 1)[0] + '\n}\n'


HELPERS = SOURCE.split('# BEGIN ASC signing helpers.', 1)[1].split('# END ASC signing helpers.', 1)[0]
HELPERS += ''.join(function(name) for name in (
    '_safe_expected', '_valid_team_id', '_valid_bundle_id', '_xml_escape',
    '_runner_signature_valid', '_command_matches_expected', '_run_runner_prebuild',
    '_ensure_launchable_runner',
))
RESTORE = SOURCE.split('# Restore the saved trio', 1)[1]
RESTORE = RESTORE[RESTORE.index('\nif '):].split('\nfi\n', 1)[0] + '\nfi\n'


class AscSigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='wda-asc-test-')
        self.root = Path(self.temp.name)
        self.helpers = self.root / 'helpers.sh'
        self.helpers.write_text(HELPERS)
        self.calls = self.root / 'calls.jsonl'
        self.status = self.root / 'status.json'

    def tearDown(self):
        self.temp.cleanup()

    def run_shell(self, body, asc=None):
        harness = self.root / 'run.sh'
        harness.write_text('''set -eu
. "$HELPERS"
WDA_UDID=00008110-001234567890001E
TEAM_ID=ABCDE12345
WDA_BUNDLE_ID=com.example.wda
WDA_DIR="$FIXTURE_DIR"
STATE_DIR="$FIXTURE_DIR"
WDA_RUNNER_NAME=iPhoneUse
WDA_XCTESTRUN=""
WDA_ICON_BUILD_LOCKED=0
XCODEBUILD_BIN=fake_xcodebuild
fake_xcodebuild() {
    "$FIXTURE_PYTHON" -c 'import json,os,sys; open(os.environ["CALLS"], "a").write(json.dumps(sys.argv[1:])+"\\n")' "$@"
    case " $* " in
        *" -showBuildSettings "*)
            "$FIXTURE_PYTHON" -c 'import json,os; print(json.dumps([{"target":"WebDriverAgentRunner","buildSettings":{"BUILT_PRODUCTS_DIR":os.environ["FIXTURE_DIR"]+"/Build/Products/Debug-iphoneos"}}]))'
            ;;
    esac
}
_setstatus() {
    "$FIXTURE_PYTHON" -c 'import json,os,sys; json.dump(dict(zip(("phase","blocked_on","message"),sys.argv[1:])),open(os.environ["STATUS_LOG"],"w"))' "$@"
}
die() { printf '%s\\n' "$*" >&2; exit 1; }
dump_runner() { "$FIXTURE_PYTHON" -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$RUNNER_ARGS" "${RUNNER_ARGV[@]}"; }
''' + body + '\n')
        env = {k: v for k, v in os.environ.items() if k not in ASC_NAMES}
        env.update(asc or {})
        env.update(HELPERS=str(self.helpers), FIXTURE_DIR=str(self.root),
                   FIXTURE_PYTHON=sys.executable, CALLS=str(self.calls), STATUS_LOG=str(self.status))
        return subprocess.run(['/bin/bash', str(harness)], env=env, cwd=self.root,
                              capture_output=True, text=True, timeout=10)

    def suffix(self, asc=FAKE_ASC):
        return ['-authenticationKeyPath', asc['WDA_ASC_KEY_PATH'],
                '-authenticationKeyID', asc['WDA_ASC_KEY_ID'],
                '-authenticationKeyIssuerID', asc['WDA_ASC_ISSUER_ID'],
                '-allowProvisioningDeviceRegistration']

    def test_all_presence_combinations_preserve_both_runner_forms(self):
        for bits in itertools.product((False, True), repeat=3):
            asc = {key: FAKE_ASC[key] for key, present in zip(ASC_NAMES, bits) if present}
            for icon in (False, True):
                with self.subTest(bits=bits, icon=icon):
                    path = '/fixture/Build/Products/WebDriverAgentRunner_iphoneos.xctestrun'
                    body = ('WDA_XCTESTRUN=' + path + '\n' if icon else '')
                    result = self.run_shell(body + '_prepare_runner_args\ndump_runner', asc)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    text, *argv = json.loads(result.stdout)
                    base = (['-destination', 'platform=iOS,id=00008110-001234567890001E',
                             'test-without-building', '-xctestrun', path] if icon else
                            ['-project', 'WebDriverAgent.xcodeproj', '-scheme', 'WebDriverAgentRunner',
                             '-destination', 'platform=iOS,id=00008110-001234567890001E',
                             '-allowProvisioningUpdates', 'DEVELOPMENT_TEAM=ABCDE12345',
                             'PRODUCT_BUNDLE_IDENTIFIER=com.example.wda', 'test'])
                    if all(bits):
                        if icon:
                            base += ['-allowProvisioningUpdates']
                        base += self.suffix()
                    self.assertEqual(argv, base)
                    self.assertEqual(text, ' '.join(argv))

    def test_all_xcode_actions_receive_one_complete_signing_suffix(self):
        for action in ('build-for-testing', 'test', 'test-without-building', '-showBuildSettings', '-showdestinations'):
            for original_updates in (False, True):
                with self.subTest(action=action, updates=original_updates):
                    self.calls.unlink(missing_ok=True)
                    body = '_wda_xcodebuild ' + ('-allowProvisioningUpdates ' if original_updates else '') + action
                    result = self.run_shell(body, FAKE_ASC)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    argv = json.loads(self.calls.read_text())
                    self.assertEqual(argv[-7:], self.suffix())
                    self.assertEqual(argv.count('-allowProvisioningUpdates'), 1)

    def test_real_prebuild_and_settings_helpers_forward_signing(self):
        result = self.run_shell('''_run_runner_prebuild "$FIXTURE_DIR/build.log"
_repair_runner_if_invalid() { return 0; }
_ensure_launchable_runner
''', FAKE_ASC)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        # Missing products cause the guarded launch path to prebuild as well.
        self.assertEqual(len(calls), 3)
        self.assertIn('build-for-testing', calls[0])
        self.assertIn('-showBuildSettings', calls[1])
        self.assertIn('build-for-testing', calls[2])
        for argv in calls:
            self.assertEqual(argv[-7:], self.suffix())

    def test_key_path_is_one_literal_argument(self):
        asc = {**FAKE_ASC, 'WDA_ASC_KEY_PATH': str(self.root / "spaces ' $(touch MUST_NOT_EXIST) `false`.p8")}
        result = self.run_shell('_prepare_runner_args\n_wda_xcodebuild "${RUNNER_ARGV[@]:0:10}"', asc)
        self.assertEqual(result.returncode, 0, result.stderr)
        argv = json.loads(self.calls.read_text())
        self.assertEqual(argv[argv.index('-authenticationKeyPath') + 1], asc['WDA_ASC_KEY_PATH'])
        self.assertFalse((self.root / 'MUST_NOT_EXIST').exists())

    def test_invalid_complete_configuration_fails_without_echoing_values(self):
        for field, value in (('WDA_ASC_KEY_PATH', 'relative/private.p8'),
                             ('WDA_ASC_KEY_PATH', '/fixture/line\nbreak.p8'),
                             ('WDA_ASC_KEY_ID', 'INVALID ID'),
                             ('WDA_ASC_ISSUER_ID', 'INVALID|ISSUER')):
            with self.subTest(field=field):
                result = self.run_shell('_prepare_runner_args', {**FAKE_ASC, field: value})
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn(value, result.stdout + result.stderr)
                self.assertFalse(self.calls.exists())

    def test_missing_account_message_is_generic_and_preserves_blocker(self):
        result = self.run_shell('_report_missing_xcode_account', FAKE_ASC)
        self.assertEqual(result.returncode, 1)
        status = json.loads(self.status.read_text())
        self.assertEqual((status['phase'], status['blocked_on']), ('signing-fail', 'account'))
        visible = result.stdout + result.stderr + self.status.read_text()
        for name in ASC_NAMES:
            self.assertIn(name, visible)
        for value in FAKE_ASC.values():
            self.assertNotIn(value, visible)

    def test_modern_and_legacy_identity_accept_both_forms_with_and_without_key(self):
        for asc in ({}, FAKE_ASC):
            for icon in (False, True):
                with self.subTest(asc=bool(asc), icon=icon):
                    body = ('WDA_XCTESTRUN=/fixture/Build/Products/WebDriverAgentRunner_iphoneos.xctestrun\n' if icon else '')
                    result = self.run_shell(body + '''_prepare_runner_args
command="/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild $RUNNER_ARGS"
_command_matches_expected "$command" "runner:$command"
_command_matches_expected "$command" "legacy-runner:$WDA_UDID:$TEAM_ID:$WDA_BUNDLE_ID"
if _command_matches_expected "$command" "legacy-runner:00008110-0000000000000000:$TEAM_ID:$WDA_BUNDLE_ID"; then exit 91; fi
if _command_matches_expected "$command changed" "runner:$command"; then exit 92; fi
''', asc)
                    self.assertEqual(result.returncode, 0, result.stderr)

    def test_identity_rejects_incomplete_or_extra_arguments(self):
        result = self.run_shell('''_prepare_runner_args
command="xcodebuild $RUNNER_ARGS"
for bad in "$command -quiet" "${command% -allowProvisioningDeviceRegistration}" "${command/ -authenticationKeyID FAKE123456/}"; do
    if _command_matches_expected "$bad" "runner:$bad"; then exit 91; fi
    if _command_matches_expected "$bad" "legacy-runner:$WDA_UDID:$TEAM_ID:$WDA_BUNDLE_ID"; then exit 92; fi
done
''', FAKE_ASC)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_saved_trio_is_restored_only_without_any_explicit_override(self):
        body = '''_existing_wda_env() {
case "$1" in
WDA_ASC_KEY_PATH) printf /saved/AuthKey_SAVED.p8 ;;
WDA_ASC_KEY_ID) printf SAVED ;;
WDA_ASC_ISSUER_ID) printf saved-issuer ;;
esac
}
''' + RESTORE + '\n_prepare_runner_args\ndump_runner'
        restored = self.run_shell(body)
        self.assertEqual(restored.returncode, 0, restored.stderr)
        self.assertIn('/saved/AuthKey_SAVED.p8', json.loads(restored.stdout))
        partial = self.run_shell(body, {'WDA_ASC_KEY_ID': 'EXPLICIT'})
        self.assertEqual(partial.returncode, 0, partial.stderr)
        self.assertNotIn('-authenticationKeyPath', json.loads(partial.stdout))
        disabled = self.run_shell(body, {'WDA_ASC_KEY_PATH': ''})
        self.assertEqual(disabled.returncode, 0, disabled.stderr)
        self.assertNotIn('-authenticationKeyPath', json.loads(disabled.stdout))

    def test_supervisor_persists_only_complete_key_references(self):
        install = function('_install_wda_supervisor')
        loop = '    for key in ' + install.split('    for key in ', 1)[1].split('\n    done', 1)[0] + '\n    done\n'
        body = '''WDA_REF=fixture
WDA_RUNNER_ICON=none
WDA_PORT=8100
MJPEG_PORT=9100
env_block=""
''' + loop + '''printf '<plist version="1.0"><dict>%s</dict></plist>' "$env_block"
'''
        for asc in ({}, {'WDA_ASC_KEY_PATH': FAKE_ASC['WDA_ASC_KEY_PATH']}, FAKE_ASC):
            with self.subTest(complete=len(asc) == 3):
                result = self.run_shell(body, asc)
                self.assertEqual(result.returncode, 0, result.stderr)
                saved = plistlib.loads(result.stdout.encode())
                found = {key: saved[key] for key in ASC_NAMES if key in saved}
                self.assertEqual(found, FAKE_ASC if len(asc) == 3 else {})


if __name__ == '__main__':
    unittest.main()
