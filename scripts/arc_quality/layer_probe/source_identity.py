"""Bind the observer to committed bytes, allowing Git's checkout EOL conversion."""
import hashlib
import subprocess

# Include the whole inference crate (not only MLA), both fixture generators,
# and their repository-local quantizer/reference imports. Enumerate the pinned
# tree, never the index, so assume-unchanged/skip-worktree cannot omit a file.
SOURCE_SCOPES = ('crates/arc-inference', 'scripts/arc_mla', 'scripts/arc_conformance')

MODEL_PATH = 'crates/arc-inference/src/modern/mla/model.rs'


def verify_engine_source(engine, observer, revision):
    def git(*args):
        return subprocess.check_output(['git', *args], cwd=engine)

    if git('rev-parse', 'HEAD').decode().strip() != revision:
        raise ValueError('engine checkout is not the provisional exact pin')
    # Retain index validation for staged edits/additions. This is supplemental:
    # every pinned file below is read independently of index cleanliness hints.
    for index_mode in ([], ['--cached']):
        subprocess.run(['git', 'diff', '--quiet', *index_mode, revision, '--',
                        *SOURCE_SCOPES], cwd=engine, check=True)
    canonical = git('cat-file', 'blob', revision + ':' + MODEL_PATH)
    observed = observer.read_bytes()
    if observed != canonical:
        raise ValueError('observer source differs from pinned engine Git blob')
    entries = []
    for record in git('ls-tree', '-r', '-z', revision, '--', *SOURCE_SCOPES).split(b'\0'):
        if not record:
            continue
        metadata, path = record.split(b'\t', 1)
        mode, kind, oid = metadata.split()
        if kind != b'blob' or mode not in (b'100644', b'100755'):
            raise ValueError('unsupported pinned source entry: ' + path.decode())
        entries.append((path.decode(), oid))
    # Batch immutable blobs by object ID, with length-delimited decoding. No
    # index, textconv or worktree filter supplies the comparison bytes.
    batch = subprocess.check_output(['git', 'cat-file', '--batch'], cwd=engine,
                                   input=b''.join(oid + b'\n' for _, oid in entries))
    offset = 0
    checked = {}
    for path, oid in entries:
        end = batch.index(b'\n', offset)
        actual_oid, kind, size = batch[offset:end].split()
        if actual_oid != oid or kind != b'blob':
            raise ValueError('unexpected pinned source blob: ' + path)
        start = end + 1
        end = start + int(size)
        committed = batch[start:end]
        if batch[end:end+1] != b'\n':
            raise ValueError('truncated pinned source blob: ' + path)
        offset = end + 1
        source = engine / path
        if source.is_symlink() or not source.is_file():
            raise ValueError('missing/nonregular tracked source: ' + path)
        current = source.read_bytes()
        # Permit exactly LF or its CRLF checkout representation for text only.
        # Binary payloads and already-CRLF blobs must stay byte-for-byte exact.
        crlf = (committed.replace(b'\n', b'\r\n')
                if b'\0' not in committed and b'\r' not in committed else committed)
        if current not in (committed, crlf):
            raise ValueError('engine worktree source differs beyond checkout line endings: ' + path)
        checked[path] = {'git_blob_sha256': hashlib.sha256(committed).hexdigest(),
                         'worktree_sha256': hashlib.sha256(current).hexdigest(),
                         'git_blob_bytes': len(committed), 'worktree_bytes': len(current),
                         'checkout_form': 'canonical' if current == committed else 'crlf'}
    if offset != len(batch) or MODEL_PATH not in checked:
        raise ValueError('incomplete pinned source inventory')
    worktree = (engine / MODEL_PATH).read_bytes()
    digest = lambda data: hashlib.sha256(data).hexdigest()
    return {'engine_sha': revision, 'model_path': MODEL_PATH,
            'identity_basis': 'all scoped tracked worktree files independently checked against pinned Git blobs; staged edits also rejected',
            'source_scopes': list(SOURCE_SCOPES), 'source_file_count': len(checked),
            'source_files': checked,
            'observer_sha256': digest(observed), 'git_blob_sha256': digest(canonical),
            'worktree_sha256': digest(worktree), 'git_blob_bytes': len(canonical),
            'worktree_bytes': len(worktree), 'worktree_crlf_count': worktree.count(b'\r\n')}
