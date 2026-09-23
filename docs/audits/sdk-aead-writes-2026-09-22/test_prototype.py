import unittest
import prototype as p

class SelectionTests(unittest.TestCase):
    def test_canonical_numeric_order_and_minimum(self):
        rows=p.candidates('\n'.join(['a refs/tags/tutanota-release-359.260904.0','b refs/tags/tutanota-release-360.260901.0','c refs/tags/tutanota-release-357.1.0','d refs/tags/tutanota-release-360.260901.0-beta','e refs/tags/other-999.1.0']))
        self.assertEqual([r['sha'] for r in rows],['b','a'])
    def test_annotated_tag_uses_peeled_commit(self):
        rows=p.candidates('a refs/tags/tutanota-release-359.260904.0\nb refs/tags/tutanota-release-359.260904.0^{}')
        self.assertEqual(rows[0]['sha'],'b')
    def test_incomplete_checks_do_not_pass(self):
        self.assertFalse(p.eligible({'checks': {'patch':'passed','build':'passed'}}))
    def test_every_gate_is_required(self):
        for key in p.REQUIRED:
            checks=dict.fromkeys(p.REQUIRED,'passed');checks[key]='failed'
            self.assertFalse(p.eligible({'checks':checks}), key)
    def test_fallback_to_next_passing_release_and_stop(self):
        calls=[]
        def attempt(row):
            calls.append(row)
            checks=dict.fromkeys(p.REQUIRED,'passed')
            if row==1: checks['protocol']='failed'
            return {'checks':checks,'id':row}
        selected,reports=p.choose([1,2,3],attempt)
        self.assertEqual(selected['id'],2);self.assertEqual(calls,[1,2])
    def test_all_fail_no_candidate(self):
        selected,reports=p.choose([1,2],lambda _: {'checks': {'patch':'failed'}})
        self.assertIsNone(selected);self.assertEqual(len(reports),2)
    def test_limit_and_tag_filter(self):
        catalog='a refs/tags/tutanota-release-359.260904.0\nb refs/tags/tutanota-release-360.260901.0'
        self.assertEqual(len(p.candidates(catalog,limit=1)),1)
        self.assertEqual(p.candidates(catalog,['tutanota-release-359.260904.0'])[0]['sha'],'a')


class CapabilityTests(unittest.TestCase):
    def test_known_limitation_does_not_hide_regression(self):
        baseline = {'optional_empty':'supported', 'aead_v3':'unsupported', 'aead_v2':'supported'}
        same = p.assess_capabilities(baseline, baseline)
        self.assertTrue(same['passed'])
        self.assertEqual(same['known_limitations'], ['aead_v3'])
        regressed = p.assess_capabilities(baseline, {'optional_empty':'unsupported','aead_v3':'unsupported','aead_v2':'supported'})
        self.assertFalse(regressed['passed'])
        self.assertEqual(regressed['regressions'], ['optional_empty'])
    def test_new_support_and_strict_requirement(self):
        baseline = {'optional_empty':'supported', 'aead_v3':'unsupported', 'aead_v2':'supported'}
        improved = p.assess_capabilities(baseline, dict.fromkeys(p.PROTOCOL_CASES,'supported'))
        self.assertTrue(improved['passed'])
        self.assertEqual(improved['improvements'], ['aead_v3'])
        self.assertFalse(p.assess_capabilities(baseline,baseline,['aead_v3'])['passed'])
    def test_missing_and_inconclusive_checks_block_selection(self):
        good = dict.fromkeys(p.PROTOCOL_CASES,'supported')
        for value in [None,'error','ignored','unsupported']:
            with self.subTest(value=value):
                current = dict(good, aead_v3=value)
                self.assertFalse(p.assess_capabilities(good,current)['passed'])
        self.assertFalse(p.assess_capabilities({},good)['passed'])
    def test_test_execution_must_be_proven(self):
        test, reason = p.PROTOCOL_CASES['aead_v3']
        ok = f'running 1 test\ntest {test} ... ok\n1 passed; 0 failed'
        known = f'running 1 test\n{reason}\ntest {test} ... FAILED\n0 passed; 1 failed'
        self.assertEqual(p.protocol_outcome('aead_v3',0,ok),'supported')
        self.assertEqual(p.protocol_outcome('aead_v3',101,known),'unsupported')
        for code, output in [(0,'running 0 tests'),(101,'error: could not compile'),(0,known),(101,known.replace(reason,'unrelated panic')),(101,known.replace('running 1 test','running 0 tests'))]:
            with self.subTest(output=output):
                self.assertEqual(p.protocol_outcome('aead_v3',code,output),'error')

    def test_unassessed_baseline_requires_a_positive_result(self):
        baseline = dict.fromkeys(p.PROTOCOL_CASES,'supported')
        baseline['aead_v2'] = 'not_assessed'
        current = dict.fromkeys(p.PROTOCOL_CASES,'supported')
        result = p.assess_capabilities(baseline,current)
        self.assertTrue(result['passed'])
        self.assertEqual(result['newly_verified'],['aead_v2'])
        for value in ['unsupported','error','not_assessed',None]:
            self.assertFalse(p.assess_capabilities(baseline,dict(current,aead_v2=value))['passed'])

if __name__=='__main__':unittest.main()
