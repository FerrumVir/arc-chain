"""Bind the observer to committed bytes, allowing Git's checkout EOL conversion."""
import hashlib
import subprocess

MODEL_PATH = 'crates/arc-inference/src/modern/mla/model.rs'


def verify_engine_source(engine, observer, revision):
    def git(*args):
        return subprocess.check_output(['git', *args], cwd=engine)

    if git('rev-parse', 'HEAD').decode().strip() != revision:
        raise ValueError('engine checkout is not the provisional exact pin')
    # Check both staged and unstaged changes against the pinned commit. Git
    # accounts for its checkout filters; substantive worktree edits still fail.
    subprocess.run(['git', 'diff', '--quiet', revision, '--',
                    'scripts/arc_mla', 'crates/arc-inference'], cwd=engine, check=True)
    canonical = git('cat-file', 'blob', revision + ':' + MODEL_PATH)
    observed = observer.read_bytes()
    if observed != canonical:
        raise ValueError('observer source differs from pinned engine Git blob')
    worktree = (engine / MODEL_PATH).read_bytes()
    # Do not trust index stat/assume-unchanged hints for the compiled model.
    # Permit only exact committed bytes or the exact LF-to-CRLF checkout form;
    # no whitespace stripping or arbitrary clean filter can hide a code edit.
    if worktree not in (canonical, canonical.replace(b'\n', b'\r\n')):
        raise ValueError('engine worktree source differs beyond checkout line endings')
    digest = lambda data: hashlib.sha256(data).hexdigest()
    return {'engine_sha': revision, 'model_path': MODEL_PATH,
            'identity_basis': 'exact pinned Git blob; clean staged/unstaged source required',
            'observer_sha256': digest(observed), 'git_blob_sha256': digest(canonical),
            'worktree_sha256': digest(worktree), 'git_blob_bytes': len(canonical),
            'worktree_bytes': len(worktree), 'worktree_crlf_count': worktree.count(b'\r\n')}
