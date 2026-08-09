# Contributing Guide

Two workflows for a reproducible Rust dev environment — pick the one that fits your tooling. Both
build the same image (`.devcontainer/Dockerfile.dev`) and land you in a shell as user `dev`.

```bash
git clone https://github.com/akvize/reconcile-rs.git && cd reconcile-rs
```

## 🔧 IDE Workflow (Dev Containers)

**Prerequisites:** Docker, and one of VS Code (**Remote – Containers** extension), GitHub
Codespaces, a JetBrains IDE with the Dev Containers plugin, or the Dev Containers CLI
(`npm install -g @devcontainers/cli`).

```bash
make dc-up
```

Runs `devcontainer up`: builds the image, creates/starts the container as user `dev`, runs
`.devcontainer/init.sh create` once (postCreate) and `.devcontainer/init.sh start` on every start
(postStart). Then open the workspace: VS Code → **Remote-Containers: Reopen in Container**;
JetBrains → **Attach to Dev Container**; CLI users land in a shell automatically. Your IDE picks up
the installed LSPs (`rust-analyzer`, `dockerfile-language-server-nodejs`, `taplo`, `marksman`)
without further setup.

To rebuild after a `Dockerfile.dev` change: `make dc-rebuild`.

## 🐳 CLI Workflow (Raw Docker)

**Prerequisites:** Docker Engine, `make`.

```bash
make dev
```

Builds the image, mounts the repo at `/workspace`, runs the same init steps as the IDE workflow, and
drops you into `bash` as user `dev`. Attach your own editor (Neovim, Emacs, …) to the running
container for LSP support — the servers above are installed under `/usr/local/bin`.

To rebuild: `make build`, or `docker build --no-cache -f .devcontainer/Dockerfile.dev -t
reconcile-rs-dev-container .` for a clean build.

## Verify (either workflow)

```bash
whoami                        # dev
git config --global user.name
git config --global user.email
ls -l .git/hooks/pre-commit   # pre-commit hook linked
rustc --version
command -v rust-analyzer dockerfile-language-server-nodejs taplo marksman
```

## Pre-commit hook

Linked automatically by `init.sh`; link manually with:

```bash
ln -sf ../../pre-commit .git/hooks/pre-commit
```

Runs [`./pre-commit`](./pre-commit) before every commit — see AGENTS.md §3 for what it checks.

## Code coverage

See [`README.md`](./README.md) "Testing and coverage" for the `cargo-llvm-cov` commands. Two extras
for local iteration: `--hide-instantiations --text` for a detailed missed-lines report, and the
`report` sub-command to reuse a previous run's results instead of re-running the tests.
