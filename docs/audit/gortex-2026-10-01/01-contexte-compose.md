# Track 1: Contexte composé

## Résumé

Comment Gortex et Pixel gèrent le contexte composé — l'agrégation de sources multiples (code, docs, historique) en un contexte unifié pour la prise de décision.

## Sources

- Gortex: documentation publique sur la composition de contexte
- Pixel: `crates/pixel/src/classify.rs` (moteur de décision), `crates/pixel-facts/src/` (index)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ `pixel classify --context` |
| Vérifiabilité | ⚠️ Partielle | ✅ `snapshot.deterministic` |
| Couverture | ✅ Multi-source | ✅ Code + docs + historique |
| Limites | ✅ Documentées | ✅ `snapshot.provider` |

## Protocole comparatif

1. Définir un problème de classification avec 3+ sources de contexte
2. Exécuter les deux systèmes avec les mêmes entrées
3. Comparer les sorties sur: précision, latence, couverture
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: composition de contexte flexible mais moins transparente sur les sources
- **Pixel**: composition explicite avec disclosure de la source (`snapshot.provider`)

## Limites

- La comparaison est limitée aux cas de classification
- Les performances ne sont pas mesurées quantitativement
