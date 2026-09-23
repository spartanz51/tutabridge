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

if __name__=='__main__':unittest.main()
