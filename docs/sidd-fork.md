# sidd fork: patches on top of upstream, shipped as a pacman repo

This fork carries a small patch stack on the `sidd` branch and republishes it
whenever upstream cuts a release.

## What is customised

| Patch | What it does |
|---|---|
| `feat(eval): add local EX-score leaderboard pane` | New Evaluation page after Machine Records: the same local machine/personal leaderboard, ranked by EX score, tagged "EX". |

Keep the stack small. Every patch is a future rebase conflict.

## How updates flow

```
pnn64/deadsync  --(new vX.Y.Z tag)-->  sidd-release workflow (every 4h)
                                         1. replay `sidd` onto the tag (throwaway checkout)
                                         2. build with upstream's Debian 12 script
                                         3. package as deadsync-sidd, build pacman db
                                         4. upload to the `repo` release
box: pacman -Syu / paru  <------------  https://github.com/sidd/deadsync/releases/download/repo
```

- The workflow never pushes rebased history. `sidd` only moves when the patch
  stack itself changes.
- Version = upstream version; `pkgrel` = CI run number, so a rebuilt patch
  stack always upgrades.
- If the replay conflicts, the job opens an issue and the box stays on its
  current version. Fix: rebase `sidd` onto the new tag locally, resolve, push.
- GitHub disables scheduled workflows after 60 days without repo activity; the
  job pushes a tiny commit to a `keepalive` branch every ~20 days to prevent that.

## One-time box setup (needs the game closed, run as root-capable user)

```sh
# 1. add the repo
sudo tee -a /etc/pacman.conf <<'EOF'

[sidd]
SigLevel = Optional TrustAll
Server = https://github.com/sidd/deadsync/releases/download/repo
EOF

# 2. swap deadsync-bin for deadsync-sidd (the package `replaces` deadsync-bin)
sudo pacman -Syu
#   or, explicitly:
sudo pacman -Rdd deadsync-bin && sudo pacman -S deadsync-sidd
```

User data in `~/.local/share/deadsync` is untouched. The package installs to
`/opt/deadsync` exactly like `deadsync-bin`.

## Updating afterwards

```sh
paru            # or: sudo pacman -Syu
```

Then restart the game (a running process keeps the old binary):
`DISPLAY=:0 ~/.config/openbox/switch-game.sh deadsync`.

## Changing the patches

```sh
git switch sidd
# ...edit, commit...
git push fork sidd          # the push also triggers a build
```

To move the stack onto a new upstream release by hand:

```sh
git fetch origin --tags
git rebase v0.5.NNNN        # resolve conflicts
cargo check -p deadsync-shell
git push --force-with-lease fork sidd
```

## Security note

The repo is unsigned (`SigLevel = Optional TrustAll`); integrity relies on
HTTPS to github.com and on control of this GitHub account.
