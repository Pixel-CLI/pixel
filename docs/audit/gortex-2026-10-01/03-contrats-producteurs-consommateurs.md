# Track 3: Contrats producteurs-consommateurs

## Résumé

Comment Gortex et Pixel gèrent les contrats producteurs-consommateurs — les accords explicites entre les composants qui produisent des données et ceux qui les consomment.

## Sources

- Gortex: documentation publique sur les contrats
- Pixel: `crates/pixel-proto/src/op.rs` (opérations typées), `crates/pixel-task/src/store.rs` (contrats de tâches)
- Comparaison conduite le 2026-10-01

## Critères de validation

| Critère | Gortex | Pixel |
|---------|--------|-------|
| Reproductibilité | ✅ Étapes documentées | ✅ Contrats typés |
| Vérifiabilité | ⚠️ Partielle | ✅ `pixel-proto` typé |
| Couverture | ✅ Multi-composants | ✅ Opérations + tâches |
| Limites | ✅ Documentées | ✅ Contrats explicites |

## Protocole comparatif

1. Définir un flux producteur-consommateurs avec contrats explicites
2. Exécuter les deux systèmes avec les mêmes contrats
3. Comparer les sorties sur: respect des contrats, erreurs, couverture
4. Documenter les divergences et leurs causes

## Résultats

- **Gortex**: contrats implicites avec validation à l'exécution
- **Pixel**: contrats typés avec validation à la compilation (`pixel-proto`)

## Limites

- La comparaison est limitée aux flux de tâches
- Les performances ne sont pas mesurées quantitativement
