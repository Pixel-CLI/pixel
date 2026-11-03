# Track 5: Simulation de modifications

## Résumé

Comment Gortex et Pixel gèrent la simulation de modifications — la capacité de simuler des changements avant leur application pour évaluer leur impact.

## Sources

- Gortex: documentation publique sur la simulation
- Pixel: `crates/pixel/src/impact_read.rs` (lecture d'impact), `crates/pixel/src/structural.rs` (analyse structurelle)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ Simulation déterministe |
| Vérifiabilité | ⚠️ Partielle | ✅ `impact_read` + `structural` |
| Couverture | ✅ Multi-fichiers | ✅ Impact + structurel |
| Limites | ✅ Documentées | ✅ Limites explicites |

## Protocole comparatif

1. Définir un ensemble de modifications à simuler
2. Exécuter les deux systèmes avec les mêmes modifications
3. Comparer les sorties sur: impact, couverture, erreurs
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: simulation manuelle avec validation humaine
- **Pixel**: simulation automatique avec `impact_read` et `structural`

## Limites

- La comparaison est limitée aux cas de modification de code
- Les performances ne sont pas mesurées quantitativement
