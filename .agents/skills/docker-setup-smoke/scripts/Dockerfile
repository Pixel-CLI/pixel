# The smoke environment only: packages, the unprivileged test user and, for
# --agents, Node and the pinned agent CLIs. run.sh passes the pinned base
# image; fetching or building pixel and the checks stay in `docker run`, so a
# failed run still exports its evidence.
# The digest is its own argument so the FROM line pins by hash: a base passed
# without a digest fails the build instead of pulling whatever the tag holds.
ARG BASE_IMAGE
ARG BASE_DIGEST
FROM ${BASE_IMAGE}@sha256:${BASE_DIGEST}
# homebrew/brew defaults to its linuxbrew user; the bootstrap runs as root.
USER root
# homebrew/brew adds a third-party apt source whose key rotation breaks
# `apt-get update`; run.sh points this at a missing directory for that base.
ARG APT_SOURCE_PARTS=/etc/apt/sources.list.d/
RUN apt-get -o "Dir::Etc::SourceParts=$APT_SOURCE_PARTS" update -qq \
    && apt-get -o "Dir::Etc::SourceParts=$APT_SOURCE_PARTS" install -y -qq \
        curl ca-certificates git python3 xz-utils >/dev/null \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -U -s /bin/bash tester \
    && install -d -o tester -g tester /evidence /home/tester/.local /home/tester/.local/bin
# Empty unless --agents: Node from nodejs.org, checked against its SHASUMS256.
ARG NODE_VERSION=""
ARG AGENT_PACKAGES=""
RUN if [ -n "$NODE_VERSION" ]; then \
        case "$(uname -m)" in aarch64) arch=arm64 ;; x86_64) arch=x64 ;; *) exit 2 ;; esac; \
        tarball="node-$NODE_VERSION-linux-$arch.tar.xz"; \
        cd /tmp \
        && curl -fsSLO "https://nodejs.org/dist/$NODE_VERSION/$tarball" \
        && curl -fsSL "https://nodejs.org/dist/$NODE_VERSION/SHASUMS256.txt" | grep " $tarball\$" | sha256sum -c - \
        && tar -xJf "$tarball" -C /usr/local --strip-components 1 \
        && rm "$tarball" \
        && npm install --global --no-fund --no-audit $AGENT_PACKAGES; \
    fi
