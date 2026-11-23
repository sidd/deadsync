You maintain a small patch stack (branch `sidd`) on top of the upstream DeadSync
project. A new upstream release tag has appeared. Produce a clean, verified
rebase of our stack onto it, and retire any patch that upstream now covers.

Context
- Upstream release tag to rebase onto: `refs/tags/upstream-$TAG` (already fetched).
- The stack currently sits on the old base `$OLD_BASE`.
- Our patches are the commits in `git log refs/tags/upstream-$TAG..HEAD`, oldest
  first. `docs/sidd-fork.md` lists them. Read each commit message and diff
  (`git show <sha>`) so you know what the patch is FOR before touching it.
- Infrastructure commits (subjects starting `ci:` or `docs:`, or touching only
  `.github/`, `packaging/`, `scripts/sidd/`, `docs/`) are never dropped and
  should apply untouched.
- The title-screen "(sidd fork)" marker is our identity; never drop it.

Step 1: retire patches upstream now provides
For every feature patch, decide whether upstream has since added the same
user-visible capability. Look at what upstream changed:
`git diff --stat $OLD_BASE refs/tags/upstream-$TAG` and
`git log --oneline $OLD_BASE..refs/tags/upstream-$TAG`, then grep the upstream
tree (`git grep ... refs/tags/upstream-$TAG -- crates assets`) for the feature:
setting names, ini keys, language strings, new modules. A patch is obsolete if
upstream delivers substantially the same behaviour, even under different names
or a different UI. Partial or merely related features do NOT count: when in
doubt, keep ours and mention the overlap in your summary.
To drop a patch, remove its commit from the rebase (e.g.
`GIT_SEQUENCE_EDITOR="sed -i 's/^pick <sha>/drop <sha>/'" git rebase -i ...`
or a `git rebase --onto`), and also remove its row from the table in
`docs/sidd-fork.md`. If a later patch depended on a dropped one, port it to
upstream's version of the feature.
Note: user settings in `~/.local/share/deadsync/deadsync.ini` that our patch
introduced (e.g. `MenuMusicVolume`, `ShowVolumeIndicator`) are harmless if
upstream ignores them; do not add migration code.

Step 2: rebase and resolve
`git rebase refs/tags/upstream-$TAG` (or the interactive variant above). For each
conflict keep BOTH upstream's new code and the intent of our patch. Upstream moves
and renames things: adapt our patch to the new structure rather than reverting
upstream. If upstream deleted a file we edit, find where the code went and port
the change. Finish with `GIT_EDITOR=true git rebase --continue` until done.

Step 3: verify. These must pass on the final tree:
- `cargo check -p deadsync-shell --locked`
- `cargo test -p deadsync-score -p deadsync-audio-stream -p deadsync-config --lib --locked`
If upstream's changes break our code beyond text conflicts, fix the relevant
patch (or add a small follow-up commit). Do not hand-edit Cargo.lock.

Hard rules
- Do NOT run `git push`; do not touch remotes or secrets.
- Do NOT modify `.github/workflows/`, `.github/actions/`, `scripts/sidd/` or
  `packaging/`.
- Do NOT run rustfmt / `cargo fmt`; it reformats unrelated code.
- Keep every remaining patch as small as it can be; do not "improve" upstream.
- If nothing needs changing, say so and leave the rebase result as is.

Finish with a short summary: patches dropped (and the upstream evidence), patches
that conflicted and how you resolved them, overlaps you deliberately kept, and
anything a human should double-check.
