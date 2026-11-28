#!/usr/bin/env python3
"""Open or merge the exact green dev promotion through the dedicated App."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('delivery', ROOT / 'scripts/release-control.py')
delivery = importlib.util.module_from_spec(spec); spec.loader.exec_module(delivery)
api = delivery.api
api_all = delivery.api_all
repo = f'repos/{delivery.REPO}'


def pending_candidate():
    return any(r['draft'] and r['tag_name'].startswith('v')
               for r in api_all(repo + '/releases?per_page=100'))


def latest_run(runs, dev, events):
    return next((r for r in runs if r['head_sha'] == dev and r['event'] in events
                 and r.get('path') == '.github/workflows/ci.yml' and r.get('head_branch') == 'dev'
                 and (r.get('head_repository') or {}).get('full_name') == delivery.REPO), None)


def completed_jobs(run):
    if run is None or run['status'] != 'completed' or run['conclusion'] != 'success':
        return ()
    return api_all(repo + f'/actions/runs/{run["id"]}/attempts/{run["run_attempt"]}/jobs?per_page=100', 'jobs')


def green_gate(run, jobs):
    return any(j['name'] == 'CI Gate' and j['conclusion'] == 'success' and j['head_sha'] == run['head_sha'] for j in jobs)


def run_gh(*args):
    subprocess.run(['gh', *args], check=True, timeout=300)


def promote(resume):
    if pending_candidate():
        print('Stable candidate pending: keep main frozen until author signing/publication completes.')
        return
    dev = api(repo + '/git/ref/heads/dev')['object']['sha']
    main = api(repo + '/git/ref/heads/main')['object']['sha']
    if api(repo + '/git/commits/' + dev)['tree']['sha'] == api(repo + '/git/commits/' + main)['tree']['sha']:
        print('No content difference; do not create a promotion/synchronization loop.')
        return
    runs = api_all(repo + '/actions/workflows/ci.yml/runs?branch=dev&per_page=50', 'workflow_runs')
    branch_run = latest_run(runs, dev, ('push', 'workflow_dispatch'))
    if not green_gate(branch_run, completed_jobs(branch_run)):
        print('Current dev is not green; wait for its latest CI.')
        return
    existing = api_all(repo + '/pulls?state=open&base=main&head=Energy-Quant:dev')
    if not existing:
        run_gh('pr', 'create', '--repo', delivery.REPO, '--base', 'main', '--head', 'dev',
               '--title', 'Promote green dev to main', '--body', 'Exact dev CI: ' + branch_run['html_url'])
        return
    if len(existing) != 1:
        raise ValueError('expected one upstream dev-to-main promotion PR')
    pr = existing[0]
    if (pr.get('state') != 'open' or pr.get('draft') is not False or pr['base']['ref'] != 'main'
            or pr['base']['repo']['full_name'] != delivery.REPO or pr['head']['ref'] != 'dev'
            or pr['head']['repo']['full_name'] != delivery.REPO or pr['head']['sha'] != dev):
        raise ValueError('promotion PR must target upstream main from the exact current upstream dev')
    pr_run = latest_run(runs, dev, ('pull_request',))
    jobs = completed_jobs(pr_run)
    # Until the cleanup PR, the legacy job still owns this PR's merge.
    legacy = any(job['name'] == 'Promote dev through CI' for job in jobs)
    if legacy or not green_gate(pr_run, jobs):
        if resume and pr_run and pr_run['status'] == 'completed':
            run_gh('run', 'rerun', str(pr_run['id']), '--repo', delivery.REPO)
        elif legacy:
            print('Legacy PR promotion job owns this run; wait for the cleanup rollout.')
        else:
            print('Current promotion PR is not green; wait for its latest CI.')
        return
    number = pr['number']
    run_gh('pr', 'checks', str(number), '--repo', delivery.REPO,
           '--required', '--watch', '--fail-fast', '--interval', '10')
    if pending_candidate() or api(repo + '/git/ref/heads/dev')['object']['sha'] != dev:
        print('Promotion inputs changed; wait for the next eligible run.')
        return
    result = json.loads(delivery.gh('api', repo + f'/pulls/{number}/merge', '--method', 'PUT',
                                    '-f', 'merge_method=merge', '-f', f'sha={dev}'))
    if not result.get('merged'):
        raise ValueError('GitHub did not merge the promotion PR')
    promoted = result['sha']
    print('Promoted main:', promoted)
    try:
        delivery.gh('api', repo + '/git/refs/heads/dev', '--method', 'PATCH',
                    '-f', f'sha={promoted}', '-F', 'force=false')
        print('Fast-forwarded dev to promoted main:', promoted)
    except subprocess.CalledProcessError:
        delivery.gh('api', repo + '/merges', '--method', 'POST', '-f', 'base=dev', '-f', 'head=main',
                    '-f', 'commit_message=Sync promoted main into dev')
        print('Merged promoted main into dev after concurrent dev advance.')


if __name__ == '__main__':
    promote(os.environ.get('RESUME_PROMOTION') == '1')
