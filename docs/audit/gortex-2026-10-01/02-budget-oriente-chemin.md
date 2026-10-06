# Track 2: Budget orienté chemin

## Résumé

Comment Gortex et Pixel gèrent le budget orienté chemin — l'allocation de ressources (tokens, temps, appels) en fonction du chemin de décision plutôt qu'un budget global.

## Sources

- Gortex: documentation publique sur le budget orienté chemin
- Pixel: `crates/pixel/src/classify.rs` (caps séparés pour state, context, criteria)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ Caps configurables |
| Vérifiabilité | ⚠️ Partielle | ✅ Disclosure des caps |
| Couverture | ✅ Multi-étapes | ✅ State/context/criteria séparés |
| Limites | ✅ Documentées | ✅ Caps avec disclosure |

## Protocole comparatif

1. Définir un problème avec des contraintes de budget strictes
2. Exécuter les deux systèmes avec les mêmes contraintes
3. Comparer les sorties sur: respect du budget, qualité, couverture
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: budget global avec ajustement dynamique
- **Pixel**: caps séparés avec disclosure (`--context`, `--criteria`, `--state`)

## Limites

- La comparaison est limitée aux cas de classification
- Les performances ne sont pas mesurées quantitativement
