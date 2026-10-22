from copy import deepcopy
import datetime as dt
import unittest

from scripts.audit_dependencies import assess, reachable_packages


class AuditReviewTests(unittest.TestCase):
    def fixture(self):
        warning = {'advisory': {'id': 'RUSTSEC-2025-0100'}, 'package': {'name': 'unic-ucd-ident', 'version': '0.9.0'}}
        report = {'vulnerabilities': {'found': False, 'list': []}, 'warnings': {'unmaintained': [warning]}}
        review = {'advisory': 'RUSTSEC-2025-0100', 'package': 'unic-ucd-ident', 'version': '0.9.0',
                  'warning': 'unmaintained', 'disposition': 'pending-owner-acceptance', 'expires': '2026-11-15'}
        return report, review

    def test_pending_and_expired_reviews_cannot_pass(self):
        report, review = self.fixture()
        now = dt.date(2026, 9, 16)
        self.assertTrue(assess(report, [review], set(), now)[0])
        review['disposition'] = 'accepted-unmaintained'
        self.assertTrue(assess(report, [review], set(), now)[0])
        review['disposition'] = 'outside-supported-graphs'
        self.assertFalse(assess(report, [review], set(), now)[0])
        self.assertTrue(assess(report, [review], set(), dt.date(2026, 11, 15))[0])

    def test_new_versions_advisories_warning_types_and_vulnerabilities_fail(self):
        original, review = self.fixture()
        review['disposition'] = 'outside-supported-graphs'
        now = dt.date(2026, 9, 16)
        for mutation in ['version', 'advisory', 'unsound', 'vulnerability']:
            report = deepcopy(original)
            warning = report['warnings']['unmaintained'][0]
            if mutation == 'version': warning['package']['version'] = '0.9.1'
            if mutation == 'advisory': warning['advisory']['id'] = 'RUSTSEC-2099-0001'
            if mutation == 'unsound': report['warnings'] = {'unsound': [warning]}
            if mutation == 'vulnerability': report['vulnerabilities'] = {'found': True, 'list': [{}]}
            with self.subTest(mutation=mutation):
                self.assertTrue(assess(report, [review], set(), now)[0])

    def test_out_of_scope_review_is_invalidated_by_dependency_reachability(self):
        report, review = self.fixture()
        review['disposition'] = 'outside-supported-graphs'
        now = dt.date(2026, 9, 16)
        self.assertFalse(assess(report, [review], set(), now)[0])
        self.assertTrue(assess(report, [review], {('unic-ucd-ident', '0.9.0')}, now)[0])
        metadata = {'workspace_members': ['root'], 'packages': [
            {'id': 'root', 'name': 'app', 'version': '1'}, {'id': 'dep', 'name': 'dep', 'version': '2'},
            {'id': 'unused', 'name': 'unused', 'version': '3'}], 'resolve': {'nodes': [
                {'id': 'root', 'deps': [{'pkg': 'dep'}]}, {'id': 'dep', 'deps': []}, {'id': 'unused', 'deps': []}]}}
        self.assertEqual(reachable_packages(metadata), {('app', '1'), ('dep', '2')})


if __name__ == '__main__':
    unittest.main()
