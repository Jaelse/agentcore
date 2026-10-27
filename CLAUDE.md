# agentcore

## Branching

Every new feature, fix or enhancement starts on its own branch, cut from the
latest `main`:

```sh
git fetch origin main
git checkout -b <short-descriptive-name> origin/main
```

- Fetch first, so the branch starts from what is on GitHub, not a stale local `main`.
- Never start from the branch that happens to be checked out (a `release/*`
  branch, another feature branch, or a branch whose PR is already merged).
- One branch, and one PR into `main`, per piece of work. Unrelated changes get
  separate branches, each cut from `main`.
- Do not commit straight to `main` or to `release/*` branches.

## Pull requests and releases

Every PR bumps the version, and every merged PR is released.

1. Branch from the latest `main`, as above.
2. Make the change, then bump the version on the same branch to the next one
   (`0.1.0-beta.9` → `0.1.0-beta.10`) in all four places, so they agree:
   - `Cargo.toml`: `version` under `[workspace.package]` (the crates inherit it)
   - `Cargo.lock`: run `cargo update --workspace`
   - `web/package.json`: `version`
   - `web/package-lock.json`: run `npm install --package-lock-only` in `web/`
3. Open the PR into `main`, and say in its description which version it releases.
4. Once it is merged, tag the merge commit on `main` with that version and push
   the tag:

   ```sh
   git fetch origin main
   git tag v0.1.0-beta.10 origin/main
   git push origin v0.1.0-beta.10
   ```

   `.github/workflows/release.yml` then builds and publishes the GitHub release.
   It fails if the tag does not match `Cargo.toml`. A tag with a suffix such as
   `-beta.10` is published as a pre-release.
