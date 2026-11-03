# Track 4: Classification de décision et garde de politiques

## Résumé

Comment Gortex et Pixel gèrent la classification de décision (classify::run) et la garde de politiques (guard.rs) — la classification structurée des décisions et la politique d'usage des outils avec avis post-édition, pas le réexamen de réponses.

## Sources

- Gortex: documentation publique sur la vérification
- Pixel: `crates/pixel/src/classify.rs` (classification de décision), `crates/pixel/src/guard.rs` (politique d'usage des outils et avis post-édition)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ Classification déterministe |
| Vérifiabilité | ⚠️ Partielle | ✅ `snapshot.deterministic` décrit le moteur de décision |
| Couverture | ✅ Multi-étapes | ✅ Guard (politique + avis) |
| Limites | ✅ Documentées | ✅ Limites explicites |

## Protocole comparatif

1. Définir un problème avec des décisions à classifier et des outils à limiter
2. Exécuter les deux systèmes avec les mêmes entrées
3. Comparer les sorties sur: qualité de classification, respect des politiques, avis
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: vérification manuelle avec validation humaine
- **Pixel**: classification de décision (`classify::run`) avec garde de politique (`guard.rs`)

## Limites

- `snapshot.deterministic` décrit le moteur de décision, pas le réexamen de réponses
- Les chements cités (`classify.rs`, `guard.rs`) ne valident pas les réponses terminées
- Cette comparaison concerne la classification et la garde, pas le réexamen de réponses
