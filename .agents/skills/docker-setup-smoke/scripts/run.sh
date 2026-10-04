#!/bin/sh
set -eu
# A hook or wrapper may export these; they would make `git -C` report
# another repository's checkout in the saved provenance.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
    GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_PREFIX
if [ "${1:-}" = --help ]; then
    echo "Usage: sh $0 [--agents] [vX.Y.Z | --source main | --source SHA | --pr NUMBER | --installer | --brew]"
    echo 'Default: v0.6.1. Source builds use the fetched commit, not the PR merge ref.'
    echo '--installer and --brew install the latest published release through install.sh or the tap.'
    echo '--agents then runs Claude Code, Codex and pi sessions against a scripted fake model.'
    exit 0
fi
invocation=$*
agents=0
if [ "${1:-}" = --agents ]; then
    agents=1
    shift
fi
# Pinned so a session's evidence names the agent build it exercised.
node_version=v24.21.0
agent_packages='@anthropic-ai/claude-code@2.1.285 @openai/codex@0.159.2 @earendil-works/pi-coding-agent@0.99.1'
is_sha() {
    case $1 in *[!0-9a-fA-F]*) return 1 ;; esac
    [ "${#1}" -eq 40 ]
}
is_pr() {
    case $1 in '' | 0* | *[!0-9]*) return 1 ;; esac
}
is_tag() {
    expr "x$1" : 'xv[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$' >/dev/null
}
mode=release
release=v0.6.1
source_ref=''
if [ $# -eq 2 ] && [ "$1" = --source ] && [ "$2" = main ]; then
    mode=source
    source_ref=refs/heads/main
elif [ $# -eq 2 ] && [ "$1" = --source ] && is_sha "$2"; then
    mode=source
    source_ref=$2
elif [ $# -eq 2 ] && [ "$1" = --pr ] && is_pr "$2"; then
    mode=source
    source_ref="refs/pull/$2/head"
elif [ $# -eq 1 ] && { [ "$1" = --installer ] || [ "$1" = --brew ]; }; then
    mode=${1#--}
elif [ $# -eq 1 ] && is_tag "$1"; then
    release=$1
elif [ $# -ne 0 ]; then
    echo 'Expected a release tag, --source main/SHA, --pr NUMBER, --installer or --brew' >&2
    exit 2
fi
scripts=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$scripts/../../../.." && pwd)
docker info >/dev/null
mkdir -p "$repo/target/docker-setup-smoke"
evidence=$(mktemp -d "$repo/target/docker-setup-smoke/run-XXXXXX")
container="pixel-setup-smoke-${evidence##*/}-$$"
base='debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251'
apt_source_parts=/etc/apt/sources.list.d/
bootstrap=bootstrap.sh
budget=300
user=tester
case $mode in
    source)
        release=''
        base='rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e'
        bootstrap=source.sh
        budget=1800
        ;;
    installer)
        release=latest
        bootstrap=installer.sh
        ;;
    brew)
        release=latest
        base='homebrew/brew@sha256:b0072bfdebf5934ae24b93b44a1928a88057399b3283ffa0177bb86084fdedfd'
        # Its GitHub CLI apt source fails `apt-get update` once the key rotates.
        apt_source_parts=/nonexistent
        bootstrap=brew.sh
        budget=900
        user=linuxbrew
        ;;
esac
image="pixel-setup-smoke:$mode"
if [ "$agents" = 0 ]; then
    node_version=''
    agent_packages=''
else
    image="$image-agents"
fi
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "Evidence: $evidence"
{
    printf 'checkout: %s\nmode: %s\nrelease: %s\nsource-ref: %s\nbase: %s\nuser: %s\n' \
        "$(git -C "$repo" rev-parse HEAD)" "$mode" "$release" "$source_ref" "$base" "$user"
    printf 'agents: %s\nnode: %s\nagent-packages: %s\n' "$agents" "$node_version" "$agent_packages"
    # The selectors above admit no quote or space, so single quotes spell them exactly.
    printf "command: sh '%s'" "$0"
    # shellcheck disable=SC2086 # split on purpose: validated selectors hold no space
    for arg in $invocation; do printf " '%s'" "$arg"; done
    printf '\n'
    docker version
} > "$evidence/identity.txt"
# Only the environment is built: no context is sent, pixel is fetched at run time.
# The Dockerfile takes the digest apart from the name: a base without one fails the build.
if ! docker build --progress plain \
    --build-arg "BASE_IMAGE=${base%@sha256:*}" --build-arg "BASE_DIGEST=${base#*@sha256:}" \
    --build-arg "APT_SOURCE_PARTS=$apt_source_parts" --build-arg "NODE_VERSION=$node_version" \
    --build-arg "AGENT_PACKAGES=$agent_packages" --tag "$image" - \
    < "$scripts/Dockerfile" > "$evidence/build.log" 2>&1; then
    echo 1 > "$evidence/exit-status.txt"
    cat "$evidence/build.log"
    echo 'Could not build the smoke image' >&2
    exit 1
fi
printf 'image: %s %s\n' "$image" "$(docker image inspect --format '{{.Id}}' "$image")" \
    >> "$evidence/identity.txt"
# Retain the stopped container just long enough to export failed checks too.
set +e
docker run --name "$container" \
    --cpus 4 --memory 6g \
    --mount "type=bind,src=$scripts,dst=/checks,readonly" \
    --env "PIXEL_RELEASE=$release" --env "PIXEL_SOURCE_REF=$source_ref" \
    --env "PIXEL_BOOTSTRAP=$bootstrap" --env "PIXEL_BOOTSTRAP_TIMEOUT=$budget" \
    --env "PIXEL_TEST_USER=$user" --env "PIXEL_AGENTS=$agents" "$image" \
    sh -ec 'timeout "$PIXEL_BOOTSTRAP_TIMEOUT" sh "/checks/$PIXEL_BOOTSTRAP"
        timeout 300 su - "$PIXEL_TEST_USER" -s /bin/sh -c "sh /checks/checks.sh"
        if [ "$PIXEL_AGENTS" = 1 ]; then timeout 600 su - "$PIXEL_TEST_USER" -s /bin/sh -c "sh /checks/agents.sh"; fi' \
    > "$evidence/run.log" 2>&1
status=$?
set -e
if ! docker cp "$container:/evidence/." "$evidence/"; then
    echo 'Could not export container evidence' >&2
    if [ "$status" -eq 0 ]; then status=1; fi
fi
printf '%s\n' "$status" > "$evidence/exit-status.txt"
cat "$evidence/run.log"
exit "$status"
