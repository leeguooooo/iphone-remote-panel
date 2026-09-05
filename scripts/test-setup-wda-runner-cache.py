#!/usr/bin/env python3
"""Exercise the runner product cache helpers from setup-wda.sh in a throwaway
state dir — no Xcode, no device. The bundle validator is stubbed."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()
HELPERS = SOURCE.split('# BEGIN runner product cache.', 1)[1].split('# END runner product cache.', 1)[0]


class RunnerCacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='wda-runner-cache-test-')
        self.root = Path(self.temp.name)
        self.state = self.root / 'state'
        self.state.mkdir()
        self.products = self.root / 'DerivedData' / 'Build' / 'Products' / 'Debug-iphoneos'
        self.products.mkdir(parents=True)
        (self.products / 'iPhoneUse-Runner.app').mkdir()
        # xcodebuild writes the .xctestrun beside the configuration directory,
        # not inside it — the shape that made the first cache never hit.
        self.xctestrun = self.products.parent / 'WebDriverAgentRunner_iphoneos26.5-arm64.xctestrun'
        self.xctestrun.write_text('plist')
        self.icon = self.root / 'AppIcon.icns'
        self.icon.write_bytes(b'icns')

    def tearDown(self):
        self.temp.cleanup()

    def run_bash(self, body, *, validate_ok=True, env=None):
        script = self.root / 'run.sh'
        script.write_text(
            'set -u\n'
            f'STATE_DIR="{self.state}"\n'
            'WDA_COMMIT=abc123\nWDA_BUNDLE_ID=com.example.wda\nTEAM_ID=TEAM000000\n'
            'WDA_UDID=00008150-0000000000000000\nWDA_RUNNER_NAME=iPhoneUse\n'
            f'RUNNER_ICON_SOURCE="{self.icon}"\n'
            'ok() { printf "ok: %s\\n" "$*"; }\nwarn() { printf "warn: %s\\n" "$*"; }\n'
            + ('_validate_runner_bundle() { return 0; }\n' if validate_ok else '_validate_runner_bundle() { return 1; }\n')
            + HELPERS + '\n' + body + '\n'
        )
        proc = subprocess.run(['bash', str(script)], capture_output=True, text=True, env={**os.environ, **(env or {})})
        return proc.returncode, proc.stdout.strip().splitlines()

    def write_body(self):
        return (f'WDA_ICON_PRODUCTS_DIR="{self.products}"\nWDA_XCTESTRUN="{self.xctestrun}"\n'
                '_runner_cache_write || exit 9\n')

    def read_body(self):
        return ('WDA_ICON_PRODUCTS_DIR=""\nWDA_XCTESTRUN=""\n'
                'if _runner_cache_read; then echo "hit $WDA_RUNNER_FROM_CACHE $WDA_ICON_PRODUCTS_DIR $WDA_XCTESTRUN $WDA_ICON_APP_PATH"; else echo miss; fi\n')

    def test_write_then_read_reuses_the_product(self):
        code, out = self.run_bash(self.write_body() + self.read_body())
        self.assertEqual(code, 0, out)
        record = json.loads((self.state / 'wda-runner-product.json').read_text())
        self.assertEqual(record['schema_version'], 1)
        self.assertEqual(record['xctestrun'], str(self.xctestrun))
        self.assertEqual(out[-1], f'hit 1 {self.products} {self.xctestrun} {self.products}/iPhoneUse-Runner.app')

    def test_key_change_misses(self):
        code, out = self.run_bash(self.write_body() + 'WDA_COMMIT=def456\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        # touching the icon source changes its mtime → miss as well
        code, out = self.run_bash(self.write_body() + f'touch -t 203001010000 "{self.icon}"\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)

    def test_missing_files_or_invalid_bundle_miss(self):
        code, out = self.run_bash(self.write_body() + f'rm "{self.xctestrun}"\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        code, out = self.run_bash(self.write_body() + self.read_body(), validate_ok=False)
        self.assertEqual(out[-1], 'miss', out)

    def test_write_requires_both_paths_and_refuses_symlinks(self):
        code, out = self.run_bash('WDA_ICON_PRODUCTS_DIR=""\nWDA_XCTESTRUN=""\n_runner_cache_write; echo "rc=$?"\n')
        self.assertEqual(out[-1], 'rc=1', out)
        target = self.root / 'elsewhere.json'
        (self.state / 'wda-runner-product.json').symlink_to(target)
        code, out = self.run_bash(self.write_body() + 'echo written\n')
        self.assertEqual(code, 9, out)
        self.assertFalse(target.exists())
        code, out = self.run_bash('_runner_cache_drop; echo "rc=$?"\n')
        self.assertEqual(out[-1], 'rc=1', out)
        self.assertTrue((self.state / 'wda-runner-product.json').is_symlink())

    def test_drop_removes_the_record(self):
        code, out = self.run_bash(self.write_body() + '_runner_cache_drop\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        self.assertFalse((self.state / 'wda-runner-product.json').exists())

    def test_xctestrun_inside_the_configuration_dir_is_also_accepted(self):
        inside = self.products / 'WebDriverAgentRunner_iphoneos.xctestrun'
        inside.write_text('plist')
        body = (f'WDA_ICON_PRODUCTS_DIR="{self.products}"\nWDA_XCTESTRUN="{inside}"\n'
                '_runner_cache_write || exit 9\n') + self.read_body()
        code, out = self.run_bash(body)
        self.assertEqual(code, 0, out)
        self.assertTrue(out[-1].startswith('hit 1 '), out)

    def test_xctestrun_outside_the_products_tree_is_rejected(self):
        stray = self.root / 'stray.xctestrun'
        stray.write_text('plist')
        body = (f'WDA_ICON_PRODUCTS_DIR="{self.products}"\nWDA_XCTESTRUN="{stray}"\n'
                '_runner_cache_write || exit 9\n') + self.read_body()
        code, out = self.run_bash(body)
        self.assertEqual(out[-1], 'miss', out)

    def test_record_outside_build_products_is_ignored(self):
        bogus = self.state / 'wda-runner-product.json'
        code, out = self.run_bash(self.write_body())
        record = json.loads(bogus.read_text())
        record['products_dir'] = str(self.root / 'not-products')
        record['xctestrun'] = str(self.root / 'not-products' / 'x.xctestrun')
        bogus.write_text(json.dumps(record))
        code, out = self.run_bash(self.read_body())
        self.assertEqual(out[-1], 'miss', out)


if __name__ == '__main__':
    unittest.main()
