# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Generate fr-en.tsv from compact lemma data (ADR-0032): `python3 -I generate.py fr-en.tsv`.

Token prefixes in a line's right-hand side:
  +verbe   regular -er verb          %verbe  -er verb with é -> è (gérer)
  &verbe   -er verb with e -> è      =verbe  -er verb doubling the consonant
  ~verbe   -oyer/-ayer verb (y -> i) @verbe  regular -ir verb (finir)
  *nom     noun + plural             ^adj    regular adjective (4 forms)
  plain    literal form
"""
import sys
import unicodedata

DATA = r"""
# ---------------------------------------------------------------- data, CRUD
create = +créer création créations créateur créatrice créateurs
new = nouveau nouvelle nouveaux nouvelles nouvel neuf neuve neufs neuves
update edit = +modifier modification modifications modifiable modifiables
update = maj +actualiser actualisation actualisations mise
update refresh = @rafraîchir rafraîchissement
set put = mettre mets met mettons mettez mettent mis mises
delete remove = +supprimer suppression suppressions supprimable
remove = +retirer &enlever
remove withdrawal = retrait retraits
record save = +enregistrer enregistrement enregistrements
save backup = +sauvegarder
list = *liste +lister listage
field = *champ
column = *colonne
array table = tableau tableaux
row line online = *ligne
cell = *cellule
value = *valeur
key = *clé *clef
id identifier = identifiant identifiants
item element = *élément
entry input = *entrée
object = *objet
model template = *modèle
schema = *schéma
base database = base bases
data = donnée données
index = indice indices +indexer indexation
query request = requête requêtes
request = demande demandes +demander demandeur demandeurs
search = +rechercher recherche recherches +chercher
find = +trouver trouvable
missing = introuvable introuvables ^manquant +manquer
filter = +filtrer filtre filtres filtrage
sort = +trier tri tris triage
order = ordre ordres +ordonner
paginate page = +paginer
next following = suivant suivante suivants suivantes
previous = précédent précédente précédents précédentes
first = premier première premiers premières
last = dernier dernière derniers dernières
account count = compte comptes
count = +compter comptage décompte compteur compteurs
number = numéro numéros +numéroter numérotation
number count = nombre nombres
digit number = chiffre chiffres
calculate compute = +calculer calcul calculs
sum amount = somme sommes
amount = *montant
total = totaux +totaliser
add = +ajouter ajout ajouts
insert = %insérer insertion insertions
copy = +copier copie copies
duplicate = +dupliquer duplication doublon doublons
paste = +coller
move = +déplacer déplacement déplacements
rename = +renommer
name = nom noms +nommer
merge = +fusionner fusion fusions
split = +diviser +découper découpage
split separate = +séparer séparation séparateur
group = groupe groupes +grouper +regrouper regroupement
aggregate = %agréger agrégat agrégats agrégation
compare = +comparer comparaison comparaisons
difference diff = différence différences différent différente différents différentes
change = changement changements +changer
history = historique historiques
state status = état états statut statuts
replace = +remplacer remplacement
fill = @remplir remplissage
empty clear = vide vides +vider
full complete = plein pleine pleins pleines
complete = %compléter complet complets complètement
required = obligatoire obligatoires requis requise requises
optional = facultatif facultative facultatifs facultatives optionnel optionnelle optionnels optionnelles
default = défaut défauts
custom customize = personnalisé personnalisée personnalisés personnalisées +personnaliser personnalisation
choice select = choix choisir choisis choisit choisissons choisissez choisissent choisi choisie choisies
select = +sélectionner sélection sélections sélecteur
input = saisie saisies saisir saisis saisit saisissons saisissez saisissent saisi
define = @définir définition définitions
settings parameter = paramètre paramètres réglage réglages
configure settings = +paramétrer paramétrage +configurer
include = inclure inclus inclut incluse incluses incluons incluez incluent inclusion
exclude = exclure exclus exclut exclue exclues excluons excluez excluent exclusion exclusions
contain = contenir contient contiennent contenant
content = contenu contenus
belong = appartenir appartient appartiennent appartenance
get obtain = obtenir obtiens obtient obtenons obtenez obtiennent obtenu obtenue obtenus obtenues
fetch get retrieve = %récupérer récupération
return = +retourner
return feedback = retour retours
render = rendu rendus
render return = rendre rends rend rendons rendez rendent
use = +utiliser utilisation utilisations
usage = usage usages
try attempt = ~essayer
attempt = tentative tentatives
retry = +réessayer
retry restart = +relancer
reminder retry = relance relances
# ---------------------------------------------------------------- users, accounts
user = utilisateur utilisatrice utilisateurs utilisatrices usager usagers
person user = personne personnes
people = gens
profile = profil profils
member = membre membres
team = équipe équipes
organization = organisation organisations
company = entreprise entreprises société sociétés
space workspace = espace espaces
project = projet projets
owner = propriétaire propriétaires
author = auteur auteurs autrice autrices
editor publisher = éditeur éditrice éditeurs éditrices
admin administrator = administrateur administratrice administrateurs administratrices
administer manage = +administrer
manage = %gérer
manager handler = gestionnaire gestionnaires
manager = responsable responsables
employee staff = employé employée employés employées salarié salariée salariés salariées
personal staff = personnel personnelle personnels personnelles
partner = partenaire partenaires
visitor = visiteur visiteuse visiteurs visiteuses
visit = visite visites +visiter
guest invite = invité invitée invités invitées
invite = +inviter
friend = ami amie amis amies amitié
first name = prénom prénoms
nickname username = pseudo pseudos surnom surnoms
family = famille familles
phone = téléphone téléphones
mobile phone = portable portables
birth birthdate = naissance naissances
gender = sexe sexes
age = âges
country = pays
city = ville villes
street = rue rues
department = département départements
address = adresse adresses
location = emplacement emplacements localisation localisations lieu lieux
geolocation = géolocalisation +géolocaliser
coordinates contact = coordonnée coordonnées
rental lease = location locations +louer loueur loueurs
seat spot place = place places
near nearby = proche proches proximité
contact = +contacter
# ---------------------------------------------------------------- auth, permissions
login connection = connexion connexions
connect login = +connecter
logout = déconnexion déconnexions
disconnect logout = +déconnecter
signup register = inscription inscriptions inscrire inscris inscrit inscrite inscrits inscrites inscrivons inscrivez inscrivent
unsubscribe = désinscription désinscriptions désinscrire désinscris désinscrit désinscrite désinscrits désinscrites +désabonner désabonnement désabonnements
word password = mot mots
password pass = passe
password = mdp
forget = +oublier oubli oublis
reset = +réinitialiser réinitialisation réinitialisations
verify check = +vérifier vérification vérifications vérificateur
confirm = +confirmer
validate = +valider validateur validateurs
valid = valide valides validité
invalid = invalide invalides +invalider invalidation
authenticate authentication = +authentifier authentification
authorize allow = +autoriser
authorization permission = autorisation autorisations
right permission = droit droits
access = accès %accéder
reject deny = +refuser refus
reject = =rejeter rejet rejets
accept = +accepter acceptation acceptations
approve = +approuver approbation approbations
token = jeton jetons
encrypt = chiffrer chiffrent chiffrez chiffrons chiffré chiffrée chiffrés chiffrées chiffrement +crypter cryptage
decrypt = +déchiffrer déchiffrement +décrypter décryptage
hash = +hacher hachage
security secure = sécurité +sécuriser
private = privé privée privés privées confidentiel confidentielle confidentiels confidentielles
privacy = confidentialité
life lifetime = vie vies
consent = consentement consentements consentir consenti consentie consentis consenties
anonymous anonymize = anonyme anonymes +anonymiser anonymisation
gdpr = rgpd
lock = +verrouiller verrou verrous verrouillage
unlock = +déverrouiller déverrouillage
block = +bloquer blocage blocages
unblock = +débloquer déblocage
identity = identité identités
signature sign = +signer
legal = juridique juridiques légal légale légaux légales
terms = cgu cgv
compliance = conformité conforme conformes
fraud = fraude fraudes frauduleux frauduleuse frauduleuses +frauder
risk = risque risques risqué risquée
# ---------------------------------------------------------------- commerce
order command = commande commandes +commander
purchase buy = achat achats
buy purchase = &acheter
buyer = acheteur acheteuse acheteurs acheteuses
seller vendor = vendeur vendeuse vendeurs vendeuses
sell = vendre vends vend vendons vendez vendent vendu vendue vendus vendues
sale = vente ventes
customer client = client cliente clients clientes clientèle
supplier provider vendor = fournisseur fournisseurs
provide supply = @fournir
provider = prestataire prestataires prestation prestations
product = *produit
cart basket = *panier
checkout = caisse caisses
catalog = catalogue catalogues
category = catégorie catégories +catégoriser
label tag = étiquette étiquettes =étiqueter étiquetage
label = libellé libellés
brand = marque marques
mark = +marquer marquage
variant = variante variantes
size = taille tailles
color = couleur couleurs
weight = poids
quantity = quantité quantités
inventory = inventaire inventaires
store storage = +stocker stockage
shop store = boutique boutiques magasin magasins
market marketplace = marché marchés
warehouse = entrepôt entrepôts
delivery = livraison livraisons
deliver = +livrer
courier driver = livreur livreuse livreurs livreuses
shipment shipping = expédition expéditions
ship = +expédier
sender = expéditeur expéditrice expéditeurs expéditrices émetteur émetteurs
carrier = transporteur transporteurs
parcel package = colis
tracking follow = suivi suivie suivis suivies
follow = suivre suit suivons suivez suivent
price = *prix
price pricing rate = tarif tarifs tarification
discount = remise remises réduction réductions rabais
promo promotion = promotions
tax = taxe taxes impôt impôts +taxer
vat = tva
invoice bill = facture factures
invoice bill = +facturer
billing invoice = facturation
receipt receive = reçu reçue reçus reçues
receive = recevoir reçois reçoit recevons recevez reçoivent
reception receive = réception réceptions
payment pay = paiement paiements
pay = ~payer paye payent
paid = payant payante payants payantes
unpaid = impayé impayée impayés impayées
free = gratuit gratuite gratuits gratuites gratuité
refund = remboursement remboursements +rembourser remboursable remboursables
subscriber follower = abonné abonnée abonnés abonnées
subscription = abonnement abonnements
subscribe = +abonner
plan package = forfait forfaits
offer = offre offres offrir offert offerte offerts offertes
trial attempt = essai essais
renewal renew = renouvellement renouvellements =renouveler
expire expiration = +expirer expiration expirations
cancel undo = +annuler
cancellation cancel = annulation annulations
cancel terminate = résiliation résiliations +résilier
suspend = suspendre suspends suspend suspendons suspendez suspendent suspendu suspendue suspendus suspendues
suspension = suspensions
reactivate = +réactiver réactivation
active = actif actifs active actives
activate enable = +activer activation activations
disable deactivate = +désactiver désactivation
inactive = inactif inactive inactifs inactives
bank = banque banques bancaire bancaires
card map = carte cartes
wallet = portefeuille portefeuilles
repository deposit = dépôt dépôts
deposit drop = +déposer
quote estimate = devis
contract = contrat contrats contractuel contractuelle
currency = devise devises monnaie monnaies
rate = taux
fee = frais
cost = coût coûts +coûter coûteux coûteuse
balance = solde soldes
payout transfer = versement versements +verser
transfer = %transférer transfert transferts
installment = mensualité mensualités échéancier échéanciers
due deadline = échéance échéances
due = dû dus dues
margin = marge marges
profit = bénéfice bénéfices rentabilité rentable rentables
revenue income = revenu revenus
accounting = comptabilité comptable comptables
book ledger = livre livres
average = moyenne moyennes
method means = moyen moyens
percent percentage = pourcentage pourcentages pourcent
round = @arrondir arrondi
decimal = décimal décimale décimaux décimales
integer = entier entière entiers entières
float = flottant flottante flottants flottantes
boolean bool = booléen booléenne booléens booléennes
true = vrai vraie vrais vraies
false wrong = faux fausse fausses
null zero = nul nulle nuls nulles zéro
loyalty = fidélité fidélisation +fidéliser
reward = récompense récompenses +récompenser
referral referrer sponsor = parrainage parrainages parrain parrains marraine +parrainer
referee referral = filleul filleule filleuls filleules
wish wishlist = souhait souhaits +souhaiter
favorite = favori favoris favorites
recommend recommendation = +recommander recommandation recommandations
suggest = %suggérer
popular = populaire populaires popularité
trend trending = tendance tendances
review = avis
rating note = note notes notation notations
rate note = +noter
star = étoile étoiles
survey poll = sondage sondages enquête enquêtes questionnaire questionnaires
dispute = litige litiges contestation contestations +contester
claim complaint = réclamation réclamations +réclamer
complaint = plainte plaintes
fine penalty = amende amendes pénalité pénalités
chargeback = rétrofacturation rétrofacturations
stock = +réapprovisionner réapprovisionnement
# ---------------------------------------------------------------- communication
email = courriel courriels
mail email = mail mails
inbox box = boîte boîtes
messaging inbox = messagerie messageries
notify = +notifier
alert = alerte alertes +alerter
reminder callback = rappel rappels
remind call = =rappeler
call = appel appels =appeler
comment = commentaire commentaires +commenter
reply answer = réponse réponses
reply answer = répondre réponds répond répondons répondez répondent répondu répondue répondus répondues
thread feed = fil
thread child = fils
topic subject = sujet sujets
post publication = publication publications
publish = +publier
unpublish = +dépublier
draft = brouillon brouillons
recipient = destinataire destinataires
send = ~envoyer envoi envois
resend = ~renvoyer renvoi renvois
forward transfer = transmettre transmets transmet transmettez transmettent transmis transmise transmises transmission transmissions
chat conversation = discussion discussions +discuter clavardage
like = aimer aime aimes aiment aimez aimons aimé aimée aimés aimées
share = +partager partage partages
mention = +mentionner
reaction react = réactions @réagir
report flag = +signaler signalement signalements
moderate moderation = modération %modérer modérateur modératrice modérateurs modératrices
ban = @bannir bannissement bannissements
spam unwanted = indésirable indésirables
abuse = abus abusif abusive abusifs abusives
flag = drapeau drapeaux
warning = avertissement avertissements
warn warning = @avertir
announcement ad listing = annonce annonces +annoncer
ad advertising = publicité publicités pub pubs
campaign = campagne campagnes
newsletter letter = lettre lettres
target targeting = cible cibles +cibler ciblage
audience = audiences
bounce = rebond rebonds +rebondir
call meeting = réunion réunions
appointment = rdv
event = événement événements évènement évènements
calendar = calendrier calendriers
booking reservation = réservation réservations
book reserve = +réserver
slot = créneau créneaux
available availability = disponible disponibles disponibilité disponibilités
unavailable = indisponible indisponibles indisponibilité indisponibilités
room channel = salon salons salle salles
channel = canal canaux
string channel chain = chaîne chaînes
character char = caractère caractères
text = texte textes textuel textuelle
title = titre titres
summary summarize = résumé résumés +résumer
body = corps
translate translation = traduire traduis traduit traduisons traduisez traduisent traduite traduits traduites traduction traductions traducteur
language locale = langue langues langage langages
welcome home = accueil accueillir accueille accueilles accueillent accueillez accueilli accueillie accueillis accueillies
welcome = bienvenue
# ---------------------------------------------------------------- files, media
file = fichier fichiers
folder directory = dossier dossiers répertoire répertoires
upload = +téléverser téléversement téléversements
download upload = +télécharger téléchargement téléchargements
attachment part = pièce pièces
attach attachment = jointe jointes joint joints
attach join = joindre joins joignons joignez joignent
attach = +attacher
detach = +détacher
thumbnail = vignette vignettes miniature miniatures
preview = aperçu aperçus +prévisualiser prévisualisation
resize = +redimensionner redimensionnement
crop = +recadrer recadrage
compress = +compresser
decompress = +décompresser décompression
export = +exporter
import = +importer
generate = %générer génération générations
generator = générateur générateurs
convert = @convertir conversion conversions convertisseur
format = +formater formatage
parse analyze = +analyser
analysis = analyse analyses
print = +imprimer impression impressions imprimante
sheet = feuille feuilles
spreadsheet = tableur tableurs
print = imprimable
# ---------------------------------------------------------------- UI
form = formulaire formulaires
form shape format = forme formes
button = bouton boutons
link = lien liens
link bind = +lier liaison liaisons
unlink = +délier
tab = onglet onglets
window modal = fenêtre fenêtres
modal = modale modales
screen = écran écrans
dropdown select = déroulant déroulante déroulants déroulantes
checkbox = case cases
check = +cocher
uncheck = +décocher
icon = icône icônes
banner = bannière bannières
footer = pied pieds
header = entête entêtes
head header = tête têtes
side sidebar = latéral latérale latéraux latérales
bar = barre barres
breadcrumb = ariane
dashboard board = bord bords
border = bordure bordures
menu = menus
show display = +afficher affichage affichages
show = +montrer
view = vue vues
view see = voir vois voit voyons voyez voient vu vus
visibility = visibilité visibilités
visible = invisible invisibles
hide mask = +masquer masquage
cache hide = cache caches
hide = cacher cachez cachons cachent caché cachée cachés cachées
click = clic clics +cliquer
hover = survol +survoler
drag = +glisser glissement
scroll = +défiler défilement
submit = soumettre soumets soumet soumettons soumettez soumettent soumis soumise soumises soumission soumissions
dismiss ignore = +ignorer
skip = +sauter
continue = +continuer
redirect = +rediriger redirection redirections
navigate = +naviguer
navigator browser = navigateur navigateurs
expand extend = étendre étends étend étendons étendez étendent étendu étendue étendus étendues
expand = +déplier +développer
collapse fold = +replier
toggle switch = +basculer
switch = interrupteur interrupteurs commutateur
pin = +épingler épingle épingles
bookmark = signet signets
highlight = +surligner surlignage
dark = sombre sombres
light clear = clair claire clairs claires
desktop office = bureau bureaux
app = appli applis
device = appareil appareils périphérique périphériques
platform = plateforme plateformes
system = système systèmes
tool = outil outils
library = bibliothèque bibliothèques librairie librairies
dependency = dépendance dépendances
depend = dépendre dépend dépendent
package = paquet paquets
plugin extension = extensions
tutorial = tutoriel tutoriels
example sample = exemple exemples
help = faq
step = étape étapes
wizard = assistant assistants
# ---------------------------------------------------------------- search, sort, reports
stats statistics = statistique statistiques
analytics = analytique analytiques
chart graph = graphique graphiques graphe graphes
metric indicator kpi = métrique métriques indicateur indicateurs
measure = mesure mesures +mesurer
report = rapport rapports
growth = croissance
funnel = entonnoir entonnoirs
cohort = cohorte cohortes
segment = +segmenter segmentation
median = médiane médianes
minimum min = minimal minimale minimaux minimales
maximum max = maximal maximale maximaux maximales
limit = limite limites +limiter limitation limitations
cap limit ceiling = plafond plafonds +plafonner
threshold = seuil seuils
rate throughput = débit débits
throttle slow = @ralentir
slow = lent lente lents lentes lenteur lenteurs
fast quick = rapide rapides rapidité
speed = vitesse vitesses
performance perf = perfs
optimize = +optimiser optimisation optimisations
memory = mémoire mémoires
disk = disque disques
network = réseau réseaux
server = serveur serveurs
host = hôte hôtes
hosting host = hébergement hébergements +héberger hébergeur
environment = environnement environnements
# ---------------------------------------------------------------- jobs, schedules, errors
queue file = file files
task job = tâche tâches
schedule = planification planifications +planifier
schedule planning = planning plannings
schedule program = +programmer programmation
recurring recurrence = récurrent récurrente récurrents récurrentes récurrence récurrences
frequency = fréquence fréquences
interval = intervalle intervalles
delay timeout deadline = délai délais
delay late overdue = retard retards
delay postpone = +retarder +reporter
duration = durée durées
period = période périodes périodique périodiques
hour time = heure heures
time = temps
schedule hours = horaire horaires
second = seconde secondes
day = jour jours journée journées
daily = quotidien quotidienne quotidiens quotidiennes
week = semaine semaines
weekly = hebdomadaire hebdomadaires hebdo
month = mois
monthly = mensuel mensuelle mensuels mensuelles
year = an ans année années
yearly annual = annuel annuelle annuels annuelles
today = aujourd
yesterday = hier
tomorrow = demain
timezone zone = fuseau fuseaux
start = +démarrer démarrage
start begin = +commencer commencement
start beginning = début débuts
end = fin fins
finish end = finir finis finit finissons finissez finissent fini finie finies
finish complete = +terminer
stop = +arrêter arrêt arrêts
restart = +redémarrer redémarrage
launch run start = +lancer lancement
run execute = +exécuter exécution exécutions
close = +fermer fermeture fermetures
open = ouvrir ouvre ouvres ouvrons ouvrez ouvrent ouvert ouverte ouverts ouvertes ouverture ouvertures
resume retry = reprendre reprends reprend reprenons reprenez reprennent repris reprise reprises
pending wait = attente attentes
wait expect = attendre attends attend attendons attendez attendent attendu attendue attendus attendues
progress current = cours
current = courant courante courants courantes actuel actuelle actuels actuelles
fail failure = échec échecs
fail = +échouer
success succeed = succès réussite réussites réussir réussis réussit réussissons réussissez réussissent réussi réussie réussies
error = erreur erreurs erroné erronée erronés erronées
exception = exceptions
issue problem bug = problème problèmes
issue problem = souci soucis
bug = bogue bogues
crash = plantage plantages +planter
fix = +corriger correction corrections correctif correctifs
debug = +déboguer débogage
outage failure = panne pannes
broken break = cassé cassée cassés cassées +casser
health = santé
monitor watch = +surveiller surveillance
log journal = journal journaux
logging = journalisation +journaliser
trace = +tracer traçage traçabilité
listen listener = +écouter écouteur écouteurs
trigger = déclencheur déclencheurs +déclencher déclenchement
hook = crochet crochets
sync = +synchroniser synchronisation synchronisations synchro synchrone synchrones
async = asynchrone asynchrones
batch lot = lot lots
bulk mass = masse masses massif massive
process = +traiter traitement traitements processus
stream flow feed = flux
conflict = conflit conflits
fallback = repli
retry backoff = réessai réessais
timeout = expirée expirées
lock = verrouillages
cleanup clean = ~nettoyer nettoyage
purge = +purger
trash bin = corbeille corbeilles
restore = +restaurer restauration restaurations
revert return = revenir reviens revient revenons revenez reviennent
undo = défaire défais défait défaits défaite
redo = refaire refais refaits refaite
deploy deployment = ~déployer déploiement déploiements
release version = version versions
migrate = +migrer
install = +installer
uninstall = +désinstaller désinstallation
upgrade improve = +améliorer amélioration améliorations
build = construire construis construit construisons construisez construisent construite construits construites construction constructions
compile = +compiler compilateur
test = +tester testé testée testés testées
mock stub = bouchon bouchons
simulate mock = +simuler simulation simulations
coverage cover = couverture couvertures
check control = contrôle contrôles +contrôler
rule = règle règles
policy = politique politiques
constraint = contrainte contraintes
priority = priorité priorités +prioriser
level = niveau niveaux
tier level = palier paliers
tree = arbre arbres arborescence arborescences
node = nœud nœuds noeud noeuds
child children = enfant enfants
root = racine racines
branch = branche branches
step = étapes
encode = +encoder encodage
decode = +décoder décodage
serialize = +sérialiser sérialisation
escape = +échapper échappement
function feature = fonction fonctions
feature = fonctionnalité fonctionnalités
work run = +fonctionner fonctionnement
method = méthode méthodes
class = classe classes
constant = constante constantes
loop = boucle boucles
variable = variables
argument parameter = argument
command = commandes
assign = +assigner assignation +attribuer attribution attributions
attribute = attribut attributs
property = propriété propriétés
relation link = relations
reference = +référencer référencement
integrate = %intégrer
api = apis
webhook = webhooks
endpoint = terminaison
handler = gestionnaires
middleware = intergiciel
event = évènementiel événementiel
listener = auditeur
reset = remise
# ---------------------------------------------------------------- additions
public = publique publiques
archive = +archiver archivage
license = licence licences
result = résultat résultats
mail email = courrier courriers
deposit = acompte acomptes caution cautions
warranty guarantee = garantie garanties
guarantee ensure = @garantir
ensure = +assurer
insurance = assurance assurances
exchange swap = +échanger échange échanges
gift = cadeau cadeaux
debit charge = +débiter prélèvement prélèvements +prélever
collect charge = +encaisser encaissement encaissements
transfer wire = virement virements
mandate = mandat mandats
merchant = marchand marchande marchands marchandes commerçant commerçants
item article = article articles
bid auction = enchère enchères
news feed = actualité actualités
quote = +citer citation citations
repost = +republier
read = lire lis lit lisez lisent lu lue lus lues lecture lectures
reader player = lecteur lectrice lecteurs lectrices
write = écrire écris écrit écrivez écrivent écrite écrits écrites écriture écritures
write draft = +rédiger rédaction rédacteur rédacteurs
vote = +voter
path = chemin chemins
routing = routage
background = fond
scheduler = planificateur planificateurs
timer = minuteur minuteurs minuterie
timestamp = horodatage horodatages +horodater
resource = ressource ressources
container = conteneur conteneurs
foreign = étranger étrangère étrangers étrangères
primary = primaire primaires
join = jointure jointures
certificate = certificat certificats
domain = domaine domaines
vulnerability = vulnérabilité vulnérabilités faille failles
attack = attaque attaques +attaquer
bot robot = robot robots
spam = pourriel pourriels
allow permit = permettre permets permet permettez permettent permis permise permises
prevent block = +empêcher
avoid prevent = +éviter
keep = +garder
keep retain = +conserver conservation
remember store = +mémoriser mémorisation
load = +charger chargement chargements
reload recharge = +recharger rechargement
unload = +décharger
raise = &lever
catch = +attraper
capture catch = +capturer capture captures
emit = émettre émets émet émettez émettent émis émise émises émission émissions
subscription subscribe = souscrire souscris souscrit souscrite souscrits souscrites souscription souscriptions
transform convert = +transformer transformation transformations
associate link = +associer
dissociate unlink = +dissocier dissociation
link connect = +relier
init initialize = +initialiser initialisation
instantiate = +instancier instanciation
inherit inheritance = +hériter héritage
override overload = +surcharger surcharge surcharges
implement = +implémenter implémentation implémentations
refactor = +refactoriser refactorisation refacto
adjust = +ajuster ajustement ajustements
estimate = +estimer estimation estimations
forecast = prévision prévisions
increment increase = +incrémenter incrément incréments
decrement = +décrémenter
increase = +augmenter augmentation augmentations
decrease = +diminuer diminution diminutions
reduce = réduire réduis réduit réduisez réduisent réduite réduits réduites
exceed overflow = +dépasser dépassement dépassements
reach = atteindre atteins atteint atteinte atteints atteintes
freeze = &geler gel +figer
detect = +détecter détection détections
query = +interroger interrogation
view consult = +consulter consultation consultations
iterate browse = parcourir parcourt parcourez parcourent parcouru parcourue parcourus
journey path = parcours
iterate = %itérer itération itérations
loop = +boucler
defer delay = %différer
extend extension = +prolonger prolongation prolongations
erase delete clear = +effacer effacement
destroy = détruire détruis détruit détruisez détruisent détruite détruits détruites destruction
cut = +couper coupure coupures
truncate = +tronquer troncature
concat = %concaténer concaténation
normalize = +normaliser normalisation
resolve resolution = résoudre résous résout résolvez résolvent résolu résolue résolus résolues résolution résolutions
fix repair = +réparer réparation réparations
move = +bouger
disable gray = +griser
forbid deny = interdire interdis interdit interdite interdits interdites interdisez interdisent interdiction interdictions
restrict = restreindre restreins restreint restreinte restreints restreintes restriction restrictions
protect = %protéger protection protections
expose = +exposer exposition
join = rejoindre rejoins rejoint rejointe rejoints rejoignez rejoignent
leave quit = +quitter
exit output = sortir sors sortez sortent sorti sortie sorties
enter = entrer entrez entré entrés
evaluate evaluation = +évaluer évaluation évaluations
rank ranking = classer classent classez classé classée classés classées classement classements
classify = +classifier classification
censor = +censurer censure
clone = +cloner clonage
inform notify = +informer
notify warn = prévenir préviens prévient prévenez préviennent prévenu prévenue
expired stale outdated = périmé périmée périmés périmées
deprecated obsolete = obsolète obsolètes déprécié dépréciée dépréciés dépréciées
next upcoming = prochain prochaine prochains prochaines
future = futur futurs futures
temporary temp = temporaire temporaires provisoire provisoires
permanent final = définitif définitive définitifs définitives
old legacy previous = ancien ancienne anciens anciennes vieux vieille vieilles
recent = récente récents récentes
main primary = principal principale principaux principales
secondary = secondaire secondaires
external = externe externes
internal = interne internes
remote = distant distante distants distantes
single only = seul seule seuls seules
large big = grand grande grands grandes gros grosse
small = petit petite petits petites
top high = haut haute hauts hautes
bottom low = bas basse basses
left = gauche
right = droite droites
remaining = restant restante restants restantes
"""

STOP = r"""
# filler and request verbs, polite words, interjections
donne donner donnes donnez donnons moi toi stp svp sil plaît plait merci
fais faire fait faite faits faites faisons font fera ferait
peux peut pouvons pouvez peuvent pourrais pourrait pourriez pourrions pouvoir
veux veut voulons voulez veulent voudrais voudrait voudrions vouloir
faut falloir faudrait faudra fallait
dois doit devons devez doivent devrait devrais devrions devoir
sais sait savons savez savent savoir
regarde regarder regardes regardez regardons
explique expliquer expliques expliquez explication explications
dis dire dit dites disons
aide aider aides aidez aidons
besoin besoins truc trucs chose choses machin bidule
voilà voici genre bah ben bon bref euh hein ok okay oui non
salut bonjour bonsoir coucou hello
aimerais aimerait souhaiterais
prendre prends prend prenons prenez prennent pris prise
penser pense penses pensez crois croire semble sembler
marche marcher marchent
passer passé passée passés passées
aller vais vas va allons allez vont
venir viens vient venons venez viennent venu venue
tenir
sert servir
# être and avoir
être suis es est êtes sont étais était étions étiez étaient été sera serai seras serons serez seront serait seraient sois soit soient étant
avoir ai as a avons avez ont avais avait avions aviez avaient eu eue eus aura aurai auras aurons aurez auront aurait auraient aie aies ait aient ayant
# pronouns, determiners
je j tu il elle on nous vous ils elles me m te t se s lui leur leurs eux y en
le la les l un une des du de d au aux à
ce c cet cette ces ceci cela ça celui celle ceux celles
mon ma mes ton ta tes son sa ses notre nos votre vos
qui que qu quoi dont lequel laquelle lesquels lesquelles duquel auquel
quelque quelques certain certaine certains certaines plusieurs divers
# question words
comment pourquoi quand où quel quelle quels quelles combien
# prepositions, conjunctions
et ou mais donc or ni car puis ensuite alors si sinon comme
avec sans pour par sur sous dans entre vers chez depuis pendant avant après
contre selon malgré via hors parmi lors dès afin jusqu jusque
ne n pas point
# adverbs
encore toujours jamais déjà aussi très bien mal plus moins trop peu assez
beaucoup tellement vraiment juste seulement simplement exactement surtout
également notamment ainsi enfin maintenant actuellement normalement
probablement peut souvent parfois rien ici là
tout tous toute toutes même mêmes autre autres chaque chacun chacune
aucun aucune
fois moment façon manière sorte
hui
quelqu lorsque lorsqu puisque puisqu parce quoique tandis cependant pourtant
toutefois néanmoins sauf environ presque plutôt vite tôt tard bientôt abord
dessus dessous dedans ailleurs partout autant tant voire etc ex cf vs ah oh hop
connaître connais connaît comprendre comprends comprend compris comprise
suffit suffire rester reste concerne concernant concerner agit agir
nécessaire nécessaires pareil idem
"""

MAX_TARGETS = 3


def fold(s):
    return "".join(c for c in unicodedata.normalize("NFD", s)
                   if unicodedata.category(c) != "Mn").replace("œ", "oe")


def er_forms(v, present_stem=None):
    stem = v[:-2]
    ps = present_stem or stem
    if stem.endswith("g"):
        ons = stem + "eons"
    elif stem.endswith("c"):
        ons = stem[:-1] + "çons"
    else:
        ons = stem + "ons"
    del ons  # "nous" forms are rare in requests ("on" is used instead)
    return [v, ps + "e", ps + "es", ps + "ent", stem + "ez",
            stem + "é", stem + "ée", stem + "és", stem + "ées"]


def expand(tok):
    p, w = tok[0], tok[1:]
    if p == "+":
        return er_forms(w)
    if p == "%":
        stem = w[:-2]
        i = stem.rfind("é")
        return er_forms(w, stem[:i] + "è" + stem[i + 1:])
    if p == "&":
        stem = w[:-2]
        i = stem.rfind("e")
        return er_forms(w, stem[:i] + "è" + stem[i + 1:])
    if p == "=":
        stem = w[:-2]
        return er_forms(w, stem + stem[-1])
    if p == "~":
        stem = w[:-2]
        return er_forms(w, stem[:-1] + "i")
    if p == "@":
        s = w[:-2]
        return [w, s + "is", s + "it", s + "issez", s + "issent",
                s + "i", s + "ie", s + "ies"]
    if p == "*":
        if w.endswith(("s", "x", "z")):
            return [w]
        if w.endswith(("eau", "eu")):
            return [w, w + "x"]
        return [w, w + "s"]
    if p == "^":
        out = [w]
        if not w.endswith("e"):
            out += [w + "e", w + "es"]
        if not w.endswith("s"):
            out.append(w + "s")
        return out
    return [tok]


def main(out_path):
    entries = {}
    order_notes = []
    for n, line in enumerate(DATA.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        left, right = line.split("=", 1)
        targets = left.split()
        for tok in right.split():
            for form in expand(tok):
                cur = entries.setdefault(form, [])
                for t in targets:
                    if t not in cur:
                        cur.append(t)
    for form, ts in entries.items():
        if len(ts) > MAX_TARGETS:
            order_notes.append(f"truncated {form}: {ts}")
            del ts[MAX_TARGETS:]
    stops = set()
    for line in STOP.splitlines():
        if line.strip().startswith("#"):
            continue
        stops.update(line.split())
    conflicts = sorted(stops & entries.keys())
    if conflicts:
        print("stop/content conflicts:", conflicts)
        return 1
    lines = {}
    for form, ts in entries.items():
        if len(ts) == 1 and fold(form) in (ts[0], ts[0] + "s"):
            continue
        lines[form] = " ".join(ts)
    for form in stops:
        lines[form] = "-"
    for note in order_notes:
        print(note)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write("# French -> English lexicon of generic software and web-product vocabulary. "
                "Written 2026-10-14 by an isolated agent from general knowledge; "
                "license: same as the repository that includes it.\n")
        for form in sorted(lines):
            f.write(f"{form}\t{lines[form]}\n")
    print("lines:", len(lines), "dash:", sum(1 for v in lines.values() if v == "-"))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
