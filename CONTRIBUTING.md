# Contributing Guide

Thank you for your interest in contributing to this project! We provide two workflows for setting up a reproducible Rust development environment:

* **IDE Workflow** (Dev Containers) for VS Code, GitHub Codespaces, JetBrains, or the Dev Containers CLI
* **CLI Workflow** for users who prefer raw Docker commands

Choose the one that fits your tooling.

---

## 🔧 IDE Workflow (Dev Containers)

**Prerequisites**:

* Docker (Desktop or Engine)
* One of:

  * VS Code with **Remote – Containers** extension
  * GitHub Codespaces
  * JetBrains IDE with the Dev Containers plugin
  * Dev Containers CLI (`npm install -g @devcontainers/cli`)

**Setup Steps**:

1. **Clone the repository**

   ```bash
   git clone https://github.com/akvize/reconcile-rs.git
   cd reconcile-rs
   ```

2. **Start the Dev Container**

   ```bash
   make dc-up
   ```

   This runs `devcontainer up`, which will:

   * Build the image via `.devcontainer/Dockerfile.dev`
   * Create and start the container as user `dev`
   * Run the **postCreate** step (`.devcontainer/init.sh create`) once
   * Run the **postStart** step (`.devcontainer/init.sh start`) on each start

3. **Open the workspace**

   * In VS Code: **Remote-Containers: Reopen in Container**
   * In JetBrains: **Attach to Dev Container**
   * CLI users will be dropped into a shell inside the `dev` user automatically

4. **Verify** inside the container:

   ```bash
   whoami                       # should be 'dev'
   git config --global user.name
   git config --global user.email
   ls -l .git/hooks/pre-commit   # pre-commit hook linked
   rustc --version
   command -v rust-analyzer
   command -v dockerfile-language-server-nodejs
   command -v taplo
   command -v marksman
   ```

🎉 You’re all set! Your IDE will pick up the installed LSPs automatically.

---

## 🐳 CLI Workflow (Raw Docker)

**Prerequisites**:

* Docker Engine (with BuildKit disabled for verbose logs if desired)
* `make` (or run commands manually)

**Setup Steps**:

1. **Clone the repository**

   ```bash
   git clone https://github.com/akvize/reconcile-rs.git
   cd reconcile-rs
   ```

2. **Build the Docker image**

   ```bash
   make build
   ```
3. **Run the container interactively**

   ```bash
   make dev
   ```

   This mounts your code at `/workspace`, bootstraps the environment and drops you into `bash` as root.

4. **Verify** inside the same shell:

   ```bash
   whoami                       # should be 'dev'
   git config --global user.name
   git config --global user.email
   ls -l .git/hooks/pre-commit
   rustc --version
   command -v rust-analyzer
   command -v dockerfile-language-server-nodejs
   command -v taplo
   command -v marksman
   ```

> **Tip**: You can also attach your preferred editor (Neovim, Emacs, JetBrains) to the running container for LSP support.

### 🔄 Rebuilding the Image

To rebuild the Docker image without running the init scripts, run:

```bash
make build
```

This will rebuild the image using your `Dockerfile.dev`.

Alternatively, to force a no-cache build:

```bash
docker build --no-cache -f .devcontainer/Dockerfile.dev -t rust-dev-container .
```

### 🧰 Attaching an Editor

For Neovim, Emacs, or other editors, launch your editor inside the container shell with:

```bash
nvim .
```

or

```bash
emacs .
```

to pick up the LSP servers installed in `/usr/local/bin`.

---

## 📄 Pre-commit Hook

We use a Git pre-commit hook to catch linting errors early. The hook is automatically linked by `init.sh`, but you can manually link it with:

```bash
ln -sf ./.devcontainer/../pre-commit .git/hooks/pre-commit
```

This will run the [`./pre-commit`](./pre-commit) before letting you create any
commit. The goal is to detect linting errors as early as possible.

# Code Coverage

To get the code coverage, run:

```bash
$ cargo install cargo-llvm-cov
$ cargo llvm-cov
```

For a detailed report of missed lines, use:

```bash
cargo llvm-cov --hide-instantiations --text
```

You can also generate an HTML version with:

```bash
cargo llvm-cov --hide-instantiations --html
```

Use the `report` sub-command to reuse the results of the previous run instead
of running the tests again.
