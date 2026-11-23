You are maintaining a small patch stack (branch `sidd`) on top of the upstream
DeadSync project. CI tried to replay the stack onto a new upstream release tag
and hit merge conflicts. Your job is to produce a clean rebase.

Context
- Upstream release tag to rebase onto: `refs/tags/upstream-$TAG` (already fetched).
- Our patches are the commits in `git log refs/tags/upstream-$TAG..HEAD` ordered
  oldest first. Read each commit message and diff (`git show <sha>`) so you know
  what the patch is FOR before touching a conflict. `docs/sidd-fork.md` lists them.
- Commits that only touch `.github/workflows/sidd-release.yml`, `packaging/`,
  `scripts/sidd/` or `docs/` are infrastructure; they should apply untouched.

Task
1. `git rebase refs/tags/upstream-$TAG`. For each conflict, resolve it so that
   BOTH upstream's new code and the intent of our patch survive. Upstream moved
   or renamed things: adapt our patch to the new structure (new module paths,
   renamed functions, changed signatures) rather than reverting upstream.
   If upstream deleted a file our patch edits, find where that code went and
   port the change there. Never resolve by dropping upstream's changes, and
   never drop one of our patches unless upstream now provides the identical
   feature (then say so in your final message).
2. `git add` + `GIT_EDITOR=true git rebase --continue` until the rebase is done.
3. Verify. These must pass on the final tree:
   - `cargo check -p deadsync-shell --locked`
   - `cargo test -p deadsync-score -p deadsync-audio-stream -p deadsync-config --lib --locked`
   If a new upstream change breaks our code (not just a text conflict), fix our
   patch in the relevant commit or add a small follow-up commit.
   Do not edit Cargo.lock by hand; if it changes, say so.

Hard rules
- Do NOT run `git push`, do not touch remotes or secrets.
- Do NOT modify `.github/workflows/sidd-release.yml`, `scripts/sidd/*` or
  `packaging/*`.
- Do NOT run rustfmt / `cargo fmt`; it reformats unrelated code and creates
  future conflicts. Keep every patch as small as it can be.
- Do not "improve" upstream code or our patches beyond what the rebase needs.
- Finish with a short summary: which patches conflicted, how you resolved each,
  and anything a human should double-check.
