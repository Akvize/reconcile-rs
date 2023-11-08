After cloning this repository, the first thing you should do is to set-up the
pre-commit hook with the command below.

```bash
$ ln -s ../../pre-commit .git/hooks/pre-commit
```

This will run the [`./pre-commit`](./pre-commit) before letting you create any
commit. The goal is to detect linting errors as early as possible.