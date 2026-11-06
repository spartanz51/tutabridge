"""Evaluate the actual job guards for closed/open/repair plans without GitHub."""
from pathlib import Path
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[2]/'.github/workflows/release.yml'

def jobs():
    text=WORKFLOW.read_text().split('jobs:\n',1)[1]
    return dict((m[1], m[2]) for m in re.finditer(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:\n|\Z)',text,re.M|re.S))

def evaluate(job, values):
    expression=re.search(r'^    if: (.*)$',jobs()[job],re.M)[1]
    expression=expression.replace('always()', 'True').replace('cancelled()', 'False')
    expression=re.sub(r'needs\.[\w-]+\.(?:result|outputs\.[\w-]+)|inputs\.publish|github\.workflow', lambda m:repr(values.get(m[0],'')), expression)
    expression=expression.replace('&&',' and ').replace('||',' or ').replace('!', ' not ')
    # Keep != intact after translating unary negation.
    expression=expression.replace(' not =','!=')
    return eval(expression, {'__builtins__':{}}, {})

class Guards(unittest.TestCase):
    def scenario(self, mode, docker, publish=True):
        v={'inputs.publish':publish,'github.workflow':'Release','needs.source.outputs.docker':str(docker).lower(),'needs.plan.outputs.mode':mode}
        for name in jobs(): v['needs.'+name+'.result']='success'
        if not docker: v['needs.registry-preflight.result']='skipped'
        if not publish:
            for name in ('plan','registry-preflight','manual-ci'):v['needs.'+name+'.result']='skipped'
        return v

    def test_closed_switch_keeps_desktop_and_skips_all_docker_effects(self):
        v=self.scenario('desktop',False)
        for job in ('docker-amd64','docker-arm64','docker-package','docker-push','docker-verify'):
            v['needs.'+job+'.result']='skipped'
        for job in ('manual-ci','build','sign-production','publish'): self.assertTrue(evaluate(job,v),job)
        for job in ('registry-preflight','docker-amd64','docker-arm64','docker-package','docker-push','docker-verify','docker-latest'): self.assertFalse(evaluate(job,v),job)

    def test_open_switch_requires_verification_before_github_publication(self):
        v=self.scenario('build',True)
        self.assertTrue(evaluate('publish',v))
        for outcome in ('skipped','failure','cancelled'):
            v['needs.docker-verify.result']=outcome
            self.assertFalse(evaluate('publish',v))

    def test_repair_cannot_bypass_registry_preflight(self):
        v=self.scenario('repair',True)
        for job in ('build','sign-production','publish','docker-package'):v['needs.'+job+'.result']='skipped'
        for job in ('docker-push','docker-verify','docker-latest'):self.assertTrue(evaluate(job,v),job)
        v['needs.registry-preflight.result']='failure'
        for job in ('docker-push','docker-verify','docker-latest'):self.assertFalse(evaluate(job,v),job)

    def test_preview_can_build_without_activation_or_privileged_jobs(self):
        v=self.scenario('',False,False)
        for job in ('build','docker-amd64','docker-arm64','sign'):self.assertTrue(evaluate(job,v),job)
        for job in ('plan','registry-preflight','sign-production','docker-push','publish','docker-latest'):self.assertFalse(evaluate(job,v),job)

    def test_artifacts_are_always_outside_trusted_checkout(self):
        for step in WORKFLOW.read_text().split('      - uses: actions/download-artifact@')[1:]:
            step=step.split('      - ',1)[0]
            self.assertRegex(step,r'path: \$\{\{ runner.temp \}\}/')
        plan=jobs()['plan']
        self.assertIn('contents: write',plan)
        self.assertNotIn('download-artifact',plan)
        self.assertIn('git status --porcelain',plan)

    def test_desktop_checksums_are_bound_to_the_signer_job_output(self):
        sign = jobs()['sign-production']
        publish = jobs()['publish']
        self.assertIn('checksums_sha256: ${{ steps.signed.outputs.checksums_sha256 }}', sign)
        self.assertIn('id: signed', sign)
        signing = sign.split('      - name: Sign production', 1)[1].split('      - name:', 1)[0]
        self.assertLess(signing.index('release-assets.py sign'), signing.index('echo "checksums_sha256='))
        self.assertIn('sha256sum "$RUNNER_TEMP/release-assets/SHA256SUMS"', signing)
        self.assertIn('>> "$GITHUB_OUTPUT"', signing)
        self.assertIn('CHECKSUMS_SHA256: ${{ needs.sign-production.outputs.checksums_sha256 }}', publish)
        self.assertIn('--checksums-sha256 "$CHECKSUMS_SHA256"', publish)
        self.assertNotIn('sha256sum', publish)
