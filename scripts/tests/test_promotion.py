"""Exact-head promotion and legacy handoff fixtures."""
import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import call, patch


def load_promotion():
    path = Path(__file__).resolve().parents[1] / 'promote.py'
    spec = importlib.util.spec_from_file_location('promote', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PromotionTests(unittest.TestCase):
    def setUp(self):
        self.promote = load_promotion()
        self.head = 'a' * 40
        self.main_sha = 'b' * 40
        self.repo = 'repos/Energy-Quant/Iskra'
        self.branch_run = {'id': 11, 'run_attempt': 1, 'run_number': 11, 'head_sha': self.head,
                           'event': 'push', 'path': '.github/workflows/ci.yml', 'head_branch': 'dev',
                           'head_repository': {'full_name': 'Energy-Quant/Iskra'},
                           'status': 'completed', 'conclusion': 'success', 'html_url': 'https://example.test/branch'}
        self.pr_run = dict(self.branch_run, id=12, run_number=12, event='pull_request',
                           html_url='https://example.test/pr')
        self.pr = {'number': 7, 'state': 'open', 'draft': False,
                   'base': {'ref': 'main', 'repo': {'full_name': 'Energy-Quant/Iskra'}},
                   'head': {'ref': 'dev', 'sha': self.head, 'repo': {'full_name': 'Energy-Quant/Iskra'}}}

    def fixture(self, *, releases=(), runs=None, pulls=None, advance_dev=False, legacy=False):
        runs = [self.pr_run, self.branch_run] if runs is None else runs
        pulls = [self.pr] if pulls is None else pulls
        dev_reads = {'count': 0}

        def fake_api(path):
            if path == self.repo + '/git/ref/heads/dev':
                dev_reads['count'] += 1
                sha = 'c' * 40 if advance_dev and dev_reads['count'] > 1 else self.head
                return {'object': {'sha': sha}}
            if path == self.repo + '/git/ref/heads/main':
                return {'object': {'sha': self.main_sha}}
            if path == self.repo + '/git/commits/' + self.head:
                return {'tree': {'sha': 'd' * 40}}
            if path == self.repo + '/git/commits/' + self.main_sha:
                return {'tree': {'sha': 'e' * 40}}
            raise AssertionError(path)

        def fake_api_all(path, key=None):
            if path == self.repo + '/releases?per_page=100':
                return list(releases)
            if path == self.repo + '/actions/workflows/ci.yml/runs?branch=dev&per_page=50':
                self.assertEqual(key, 'workflow_runs')
                return list(runs)
            if path == self.repo + '/pulls?state=open&base=main&head=Energy-Quant:dev':
                return list(pulls)
            if '/jobs?per_page=100' in path:
                self.assertEqual(key, 'jobs')
                run_id = int(path.split('/actions/runs/')[1].split('/')[0])
                matching = next((r for r in runs if r['id'] == run_id), None)
                head = matching['head_sha'] if matching else self.head
                jobs = [{'name': 'CI Gate', 'conclusion': 'success', 'head_sha': head}]
                if legacy and matching and matching['event'] == 'pull_request':
                    jobs.append({'name': 'Promote dev through CI', 'conclusion': 'success'})
                return jobs
            raise AssertionError(path)

        return (patch.object(self.promote, 'api', side_effect=fake_api),
                patch.object(self.promote, 'api_all', side_effect=fake_api_all),
                patch.object(self.promote, 'run_gh'),
                patch.object(self.promote.delivery, 'gh'))

    def run_promotion(self, *, resume=False, **kwargs):
        api_patch, all_patch, run_patch, gh_patch = self.fixture(**kwargs)
        with api_patch, all_patch, run_patch as run_gh, gh_patch as gh:
            gh.side_effect = [json.dumps({'merged': True, 'sha': 'f' * 40}), '{}']
            self.promote.promote(resume)
        return run_gh, gh

    def test_successful_exact_head_promotion_merges_and_syncs(self):
        run_gh, gh = self.run_promotion()
        run_gh.assert_called_once_with('pr', 'checks', '7', '--repo', 'Energy-Quant/Iskra',
                                       '--required', '--watch', '--fail-fast', '--interval', '10')
        self.assertEqual(gh.call_args_list, [
            call('api', 'repos/Energy-Quant/Iskra/pulls/7/merge', '--method', 'PUT',
                 '-f', 'merge_method=merge', '-f', 'sha=' + self.head),
            call('api', 'repos/Energy-Quant/Iskra/git/refs/heads/dev', '--method', 'PATCH',
                 '-f', 'sha=' + 'f' * 40, '-F', 'force=false')])

    def test_fast_forward_conflict_falls_back_to_merge(self):
        api_patch, all_patch, run_patch, gh_patch = self.fixture()
        with api_patch, all_patch, run_patch, gh_patch as gh:
            gh.side_effect = [json.dumps({'merged': True, 'sha': 'f' * 40}),
                              subprocess.CalledProcessError(1, ['gh']), '{}']
            self.promote.promote(False)
        self.assertEqual(gh.call_args_list, [
            call('api', 'repos/Energy-Quant/Iskra/pulls/7/merge', '--method', 'PUT',
                 '-f', 'merge_method=merge', '-f', 'sha=' + self.head),
            call('api', 'repos/Energy-Quant/Iskra/git/refs/heads/dev', '--method', 'PATCH',
                 '-f', 'sha=' + 'f' * 40, '-F', 'force=false'),
            call('api', 'repos/Energy-Quant/Iskra/merges', '--method', 'POST',
                 '-f', 'base=dev', '-f', 'head=main',
                 '-f', 'commit_message=Sync promoted main into dev')])

    def assert_blocked(self, **kwargs):
        run_gh, gh = self.run_promotion(**kwargs)
        gh.assert_not_called()
        self.assertFalse(any(args.args[:2] == ('pr', 'create') for args in run_gh.call_args_list))

    def test_blocked_when_pr_ci_missing_or_not_green(self):
        missing = [self.branch_run]
        self.assert_blocked(runs=missing)
        failed = dict(self.pr_run, conclusion='failure')
        self.assert_blocked(runs=[failed, self.branch_run])

    def test_oldest_green_pr_ci_cannot_override_newer_pending(self):
        pending = dict(self.pr_run, id=13, run_number=13, status='in_progress', conclusion=None)
        self.assert_blocked(runs=[pending, self.pr_run, self.branch_run])

    def test_wrong_fork_or_head_is_rejected_before_write(self):
        wrong_fork = dict(self.pr, head=dict(self.pr['head'], repo={'full_name': 'alice/Iskra'}))
        with self.assertRaises(ValueError):
            self.run_promotion(pulls=[wrong_fork])
        wrong_head = dict(self.pr, head=dict(self.pr['head'], sha='0' * 40))
        with self.assertRaises(ValueError):
            self.run_promotion(pulls=[wrong_head])

    def test_pending_stable_candidate_blocks_before_writes(self):
        draft = {'draft': True, 'tag_name': 'v9.9.9'}
        run_gh, gh = self.run_promotion(releases=[draft])
        run_gh.assert_not_called()
        gh.assert_not_called()

    def test_dev_advancing_before_merge_blocks_write(self):
        run_gh, gh = self.run_promotion(advance_dev=True)
        gh.assert_not_called()

    def test_legacy_ci_keeps_merge_ownership(self):
        run_gh, gh = self.run_promotion(legacy=True)
        run_gh.assert_not_called()
        gh.assert_not_called()
        run_gh, gh = self.run_promotion(legacy=True, resume=True)
        run_gh.assert_called_once_with('run', 'rerun', '12', '--repo', 'Energy-Quant/Iskra')
        gh.assert_not_called()


if __name__ == '__main__':
    unittest.main()
