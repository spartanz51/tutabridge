"""Exercise the inline fallback without loading the React bundle or Tauri."""

import json
from pathlib import Path
import shutil
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(shutil.which("node"), "Node.js is required")
class WindowErrorsTest(unittest.TestCase):
    def test_errors_show_only_when_the_window_is_empty(self):
        result = subprocess.run(
            ["node", "-e", r'''
const fs = require('fs');
const vm = require('vm');
const source = fs.readFileSync('ui/index.html', 'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];
const results = [];
for (const kind of ['error', 'unhandledrejection']) {
  for (const occupied of [false, true]) {
    const handlers = {}, logs = [], panels = [], timers = [];
    vm.runInNewContext(source, {
      window: {
        addEventListener: (name, fn) => { handlers[name] = fn; },
        __TAURI_INTERNALS__: { invoke: (cmd, args) => {
          logs.push([cmd, args.message]); return Promise.resolve();
        } }
      },
      document: {
        getElementById: () => ({ childNodes: occupied ? [{}] : [] }),
        createElement: () => ({ style: {}, appendChild: () => {} }),
        body: { appendChild: node => panels.push(node) }
      },
      setTimeout: fn => timers.push(fn)
    });
    const event = kind === 'error' ? { message: 'synthetic failure' } : { reason: 'synthetic failure' };
    handlers[kind](event);
    handlers[kind](event);
    timers.forEach(fn => fn());
    results.push({ kind, occupied, logs, panels: panels.length });
  }
}
console.log(JSON.stringify(results));
'''],
            cwd=ROOT, capture_output=True, text=True, check=True,
        )
        for case in json.loads(result.stdout):
            with self.subTest(kind=case["kind"], occupied=case["occupied"]):
                self.assertEqual(case["panels"], 0 if case["occupied"] else 1)
                self.assertEqual(len(case["logs"]), 2)
                for command, message in case["logs"]:
                    self.assertEqual(command, "log_frontend_error")
                    self.assertIn("synthetic failure", message)
