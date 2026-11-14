# Releasing

This is the operational runbook for `pagh-riscv`. The branch model is in
`BRANCHING.md`; the version/feature rules are in `AGENTS.md` (`## Versioning`).
Nothing here releases automatically except the tag-driven `release` workflow
(`.github/workflows/release.yml`).

## Version source

`Cargo.toml [package].version` is the single version source. `Cargo.lock` mirrors
it and every build runs with `--locked`, so the two move in **one** commit: a
`Cargo.toml`-only bump fails the build job in seconds (`cannot update the lock
file … because --locked was passed`). Verify with `cargo build --locked`.

`dev` carries the **next release** version. The first feature after a release
bumps MINOR, the first fix bumps PATCH; docs/process-only changes do not bump.
`main` keeps the last **released** version. A stable tag `vX.Y.Z` must equal the
version in `Cargo.toml` on `main`; an RC tag `vX.Y.Z-rc.N` must equal it on `dev`
(the workflow compares the tag base — the part before `-rc.N` — so the source
file never carries an `-rc` suffix). The `AGENTS.md` "currently reads" line is
updated only when a stable release lands.

## Pre-release (release candidate) — from `dev` only

1. Freeze `dev`: after `vX.Y.Z-rc.1`, only fixes for this release are merged
   until the release lands. Everything else waits.
2. Pre-flight on the exact `dev` commit:
   - CI green (all required checks): `cargo fmt --all -- --check`, both builds
     (`cargo build --locked`, `cargo build --release --locked`) and the QEMU
     smoke suite in its six modes — serial, ramfb, keyboard, net, disk-fresh
     (format/mount/write) and disk-existing (reboot reads the file back);
   - live evidence run and linked:
     - the six-mode smoke suite on the debug kernel (`.github/smoke.py`,
       the same commands as CI);
     - `python3 tools/test_security.py` — crafted ext2 symlink and W^X/overlap
       `PT_LOAD` ELFs in QEMU, transcript `docs/live-security.txt`;
     - the in-QEMU shell `selftest` — `SELFTEST SUMMARY (user): 16/16 passed`
       with `/mnt` mounted (13/16 + 3 SKIP without a disk) and
       `SELFTEST SUMMARY (net): 12/12 passed`;
   - release notes written;
   - `Cargo.toml` version equals `X.Y.Z` and the lock is consistent
     (`cargo build --locked` clean);
   - no open blocker, no known regression in the release scope.
3. Tag and push `vX.Y.Z-rc.N`:
   ```sh
   git checkout dev && git pull --ff-only
   git tag -a vX.Y.Z-rc.1 -m "pagh-riscv X.Y.Z-rc.1"
   git push origin vX.Y.Z-rc.1
   ```
4. Watch the `release` workflow: it refuses the tag unless it is reachable from
   `dev` **and not** from `main`, checks the base version, builds the release
   kernel (`cargo build --release --locked` with the nightly from
   `rust-toolchain.toml`), names the ELF `pagh-<tag>`, writes `.sha256` for the
   kernel and the source archive, and publishes a GitHub **Pre-release**.
   A rejected tag stays in the repository but produces **no** release entry:
   delete it (`git push origin :refs/tags/<tag>`) before retrying.
5. Test the pre-release artifact: boot `pagh-vX.Y.Z-rc.N` in QEMU (OpenSBI
   `virt`) and check the `sha256sum -c pagh-vX.Y.Z-rc.N.sha256` digest. Bugs:
   fix on `dev`, tag `-rc.2` (never move the `-rc.1` tag).

## Stable release — from `main`

1. Merge `dev` into `main` (merge commit; no code changes at this step):
   ```sh
   git checkout main && git pull --ff-only
   git merge --no-ff dev
   git push origin main
   ```
2. Wait for CI on `main` to be green.
3. Tag and push `vX.Y.Z` on the merge commit; the workflow checks that the
   commit is reachable from `main` and that the version source matches.
4. Verify in **Releases**: the new entry is a full release with the versioned
   kernel (`pagh-vX.Y.Z`), its `.sha256`, the source archive
   (`pagh-riscv-vX.Y.Z.tar.gz`) and its `.sha256`. GitHub marks the newest
   stable as **Latest**.
5. `dev` continues with the next version (first change bumps it).

## Hotfix — straight to `main`, no RC

Hotfixes are limited to **critical/high verified security findings** or a
release-blocking regression in a released version. Everything else rides `dev`.

1. Branch from `main`: `hotfix/<short-name>`.
2. PR to `main` (never to `dev`), full CI plus a targeted regression test/PoC
   for the bug (for security findings: a `tools/test_security.py`-style live
   acceptance, or a committed PoC under `tools/`). Get it reviewed.
3. Merge to `main`, then tag the PATCH release `vX.Y.Z+1` directly (no RC) and
   verify the release entry.
4. Merge `main` back into `dev` with `--no-ff`:
   ```sh
   git checkout dev && git pull --ff-only
   git merge --no-ff main
   ```
   Resolve version conflicts in favour of the larger version (dev stays ahead).
   `AGENTS.md` merge additively: keep both release sections.
5. CI on `dev` must be green after the merge-back.

## Cross-repository ABI change order

`pagh-riscv` ships the runtime/ABI that userland, the SDKs and packaged images
consume. When a change touches the C/ABI boundary, release in dependency order
and only then update dependants:

1. `pagh-api` / `pagh-libc` (whichever changed; if both, `pagh-api` first when
   the SDK pins the libc header).
2. `pagh-rust` / `pagh-python` (they consume `pagh-api`/`pagh-libc`).
3. `pagh-programs` (apps built against the new SDK), `pagh-repo` (published
   packages).
4. `pagh-riscv` (and Pagh-OS) images that ship the new runtime/ABI.
5. Update the pinned sibling versions (`Cargo.toml` path/registry deps,
   `Cargo.lock`) in the same release train, not later.

Never tag a dependant before its dependency is tagged; the release workflow
only checks branch/version, not cross-repo compatibility.

## Backfill and "Latest"

GitHub marks **Latest** by release creation time, not by version number. When
backfilling already-released versions, create tags in ascending version order
(oldest first), so the newest version is created last and becomes Latest.

## Rollback / mistakes

- Wrong tag on the wrong commit: delete the release (`gh release delete <tag>
  --yes`) and the tag (`git push origin :refs/tags/<tag>`), fix, retag.
- Workflow rejected the tag: delete the tag; no release exists to clean up.
- Bad stable release: prefer a PATCH hotfix over moving/deleting a published
  tag; if the release is minutes old and nothing depends on it, delete
  release + tag, then re-tag the fixed commit.
- Never move an existing tag; immutable tags are enforced by the tag ruleset.

## Release checklist (paste into the release PR / notes)

- [ ] `Cargo.toml` version equal to the tag base; `Cargo.lock` in the same commit
- [ ] Release notes written
- [ ] CI green on the exact commit (fmt, debug/release build, six-mode QEMU smoke)
- [ ] Live evidence run and linked (smoke + `tools/test_security.py` + `selftest`)
- [ ] Freeze respected (no feature merges after `-rc.1`)
- [ ] `pagh-<tag>` ELF + `.sha256` downloaded and smoke-checked
- [ ] `main`/`dev` synchronized per the flow above
- [ ] Release entry has the right type (Pre-release vs Release)