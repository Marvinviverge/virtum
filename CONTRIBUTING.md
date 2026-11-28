# Contribuer à Virtum

**Français** | [English](./CONTRIBUTING.en.md)

Merci de votre intérêt. Les rapports de bug, les idées et les correctifs sont bienvenus. Les échanges suivent le [code de conduite](./CODE_OF_CONDUCT.fr.md).

## Signaler un problème

- **Bug ou idée** : ouvrez une issue avec la version (`virtum --version`), le système (`virtum doctor`), le Virtumfile minimal qui reproduit le problème et ce que vous attendiez.
- **Faille de sécurité** : n'ouvrez pas d'issue publique. Suivez [SECURITY.md](./SECURITY.md) (security@virtum.dev).

## Proposer un changement

1. Pour un changement important, ouvrez d'abord une issue pour en discuter.
2. Créez une branche depuis `develop` et faites des commits courts, avec un message qui dit ce qui change et pourquoi. La pull request vise `develop` ; `main` ne reçoit que les versions publiées, depuis `develop`.
3. Vérifiez avant d'ouvrir la pull request :

   ```bash
   make check        # format, clippy, tests, audit des dépendances
   ```

   Si vous touchez au réseau, aux partages ou au cycle de vie des VM, lancez aussi un vrai lab (`make smoke-lab`) et dites-le dans la pull request.
4. Ajoutez un test pour tout correctif de bug ou de sécurité : une régression doit faire échouer un test.

La CI relance ces contrôles sur chaque pull request.

## Signature des commits (DCO)

Chaque commit doit porter une ligne `Signed-off-by`, ajoutée par `git commit -s` :

```
Signed-off-by: Prénom Nom <adresse@exemple.org>
```

Par cette ligne, vous certifiez le [Developer Certificate of Origin 1.1](https://developercertificate.org/) : vous avez le droit de soumettre ce code et vous acceptez qu'il soit distribué sous la licence du projet ([Apache-2.0](./LICENSE)). Une pull request dont un commit n'est pas signé ne peut pas être fusionnée.

L'adresse de la signature peut être l'adresse « noreply » de GitHub si vous ne souhaitez pas publier la vôtre.

## Licence

En contribuant, vous acceptez que votre contribution soit publiée sous licence Apache-2.0, comme le reste du projet.
