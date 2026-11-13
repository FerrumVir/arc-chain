# Contributing to ARC

Thanks for helping. This page covers what every pull request needs before it
can be merged.

## Sign off every commit (DCO)

ARC uses the [Developer Certificate of Origin 1.1](https://developercertificate.org/)
(DCO). When you sign off a commit, you certify the statements in the DCO: in
short, that you wrote the change or otherwise have the right to submit it under
this repository's license.

Every commit in a pull request needs a `Signed-off-by` line whose name and
email match the commit's author:

```text
Signed-off-by: Your Name <you@example.com>
```

Git adds the line for you when you commit with `-s`:

```sh
git commit -s -m "Describe the change"
```

Git takes the name and email from `git config user.name` and
`git config user.email`. Set those to the identity you want on the commit. A
GitHub noreply address is fine.

Co-authors can still be credited with `Co-authored-by:` lines. The sign-off is
the author's.

### Fix a pull request that is missing a sign-off

- **Last commit only:** run `git commit --amend --no-edit -s`, then
  `git push --force-with-lease`.
- **Several commits:** run `git rebase --signoff origin/main`, then
  `git push --force-with-lease`.

### The check

The `DCO` workflow runs on every pull request to `main`. It lists, by commit
SHA, any commit without a `Signed-off-by` line that matches its author, and
fails until each one is fixed.

- It compares the name and email in the sign-off with the commit's author name
  and email, ignoring case.
- It skips merge commits, such as the ones GitHub's "Update branch" button
  creates.
- It skips pull requests opened by Dependabot, which cannot sign off. A
  maintainer reviews those before merging.
- It reads commit metadata only. It has read-only permissions, uses no secrets
  and never runs code from the pull request, so it behaves the same for pull
  requests from forks.
