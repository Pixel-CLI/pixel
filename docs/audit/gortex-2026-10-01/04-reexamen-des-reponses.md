# Track 4: Réexamen des réponses

## Résumé

Comment Gortex et Pixel gèrent le réexamen des réponses — la vérification et la validation des réponses produites par le système avant leur livraison.

## Sources

- Gortex: documentation publique sur le réexamen
- Pixel: `crates/pixel/src/classify.rs` (vérifiction), `crates/pixel/src/guard.rs` (garde)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ Vérifiction déterministe |
| Vérifiabilité | ⚠️ Partielle | ✅ `snapshot.deterministic` |
| Couverture | ✅ Multi-étapes | ✅ Guard + verify |
| Limites | ✅ Documentées | ✅ Limites explicites |

## Protocole comparatif

1. Définir un problème avec des réponses à réexaminer
2. Exécuter les deux systèmes avec les mêmes entrées
3. Comparer les sorties sur: qualité, couverture, erreurs
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: réexamen manuel avec validation humaine
- **Pixel**: réexamen automatique avec garde (`guard.rs`) et vérifiction

## Limites

- La comparaison est limitée aux cas de classification
- Les performances ne sont pas mesurées quantitativement
