#!/usr/bin/env python3
"""Run the source image smoke suite, requiring every discovered test to execute.

Invoke from the source checkout, with TUTABRIDGE_DOCKER_IMAGE and any Linux
fixture paths provided by the native build job. Historical sources use their
own suite; a missing fixture in a newer suite must never silently pass.
"""
import sys
import unittest


def main():
    suite = unittest.defaultTestLoader.discover('scripts/tests', pattern='test_docker_smoke.py')
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.testsRun or result.skipped:
        print('Docker smoke requires a nonempty suite with no skipped tests', file=sys.stderr)
        return 1
    return 0 if result.wasSuccessful() else 1


if __name__ == '__main__':
    sys.exit(main())
