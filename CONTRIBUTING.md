# Pre-Commit hook

After cloning this repository, the first thing you should do is to set-up the
pre-commit hook with the command below.

```bash
$ ln -s ../../pre-commit .git/hooks/pre-commit
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
