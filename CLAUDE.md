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
