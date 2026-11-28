<!--
English or French, as you prefer. / En anglais ou en français, au choix.
The pull request targets `develop`. / La pull request cible `develop`.
See / Voir CONTRIBUTING.en.md, CONTRIBUTING.md.
-->

## What and why / Quoi et pourquoi

<!-- What changes, and why. Link the issue if there is one (Fixes #123). -->
<!-- Ce qui change, et pourquoi. Citez l'issue s'il y en a une (Fixes #123). -->

## How it was tested / Comment c'est testé

<!-- Commands run, system (`virtum doctor`), real lab if relevant. -->
<!-- Commandes lancées, système (`virtum doctor`), vrai lab si utile. -->

## Checklist

- [ ] `make check` passes (formatting, clippy, tests, dependency audit) / `make check` passe
- [ ] Networking, shares or VM lifecycle touched: `make smoke-lab` run on a real host / Réseau, partages ou cycle de vie des VM touchés : `make smoke-lab` lancé sur un vrai hôte
- [ ] Bug or security fix: a test fails without the fix / Correctif : un test échoue sans lui
- [ ] New messages written in both languages with `tr!(fr: …, en: …)` / Nouveaux messages dans les deux langues avec `tr!(fr: …, en: …)`
- [ ] Docs updated if behavior changes (README FR/EN, CHANGELOG "Non publié") / Doc à jour si le comportement change (README FR/EN, CHANGELOG « Non publié »)
- [ ] Every commit is signed off (`git commit -s`, DCO) / Chaque commit est signé (`git commit -s`, DCO)
