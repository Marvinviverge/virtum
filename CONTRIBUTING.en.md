# Contributing to Virtum

[Français](./CONTRIBUTING.md) | **English**

Thank you for your interest. Bug reports, ideas and fixes are welcome. Exchanges follow the [code of conduct](./CODE_OF_CONDUCT.md).

## Reporting a problem

- **Bug or idea**: open an issue with the version (`virtum --version`), the system (`virtum doctor`), a minimal Virtumfile that reproduces the problem, and what you expected.
- **Security vulnerability**: do not open a public issue. Follow [SECURITY.md](./SECURITY.md) (security@virtum.dev).

## Proposing a change

1. For a significant change, open an issue first to discuss it.
2. Create a branch from `develop` and make small commits, with a message that says what changes and why. The pull request targets `develop`; `main` only receives released versions, from `develop`.
3. Before opening the pull request, check:

   ```bash
   make check        # formatting, clippy, tests, dependency audit
   ```

   If you touch networking, shares or the VM lifecycle, also run a real lab (`make smoke-lab`) and say so in the pull request.
4. Add a test for every bug or security fix: a regression must make a test fail.

CI runs these checks again on every pull request.

## Signing off commits (DCO)

Every commit must carry a `Signed-off-by` line, added by `git commit -s`:

```
Signed-off-by: First Last <address@example.org>
```

With this line you certify the [Developer Certificate of Origin 1.1](https://developercertificate.org/): you have the right to submit this code and you agree that it is distributed under the project's license ([Apache-2.0](./LICENSE)). A pull request with an unsigned commit cannot be merged.

The sign-off address can be your GitHub "noreply" address if you do not want to publish your own.

## License

By contributing, you agree that your contribution is published under the Apache-2.0 license, like the rest of the project.
