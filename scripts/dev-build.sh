#!/usr/bin/env bash
# Dev-only build helper for the build/local-testing integration branch.
# Never used for the real release image (see Dockerfile) and never merged to
# main -- this script only exists on the utility build branch.
#
# Splits the build into a cached base layer (Dockerfile.base: dependency
# compilation only) and a fast dev layer (Dockerfile.dev: real crates/
# source). The base layer is tagged by a content hash of everything that
# actually defines it, so it is rebuilt only when that content changes --
# not on every ticket-fix commit, which only touches crates/*/src.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

REGISTRY="${REGISTRY:-ghcr.io}"
IMAGE_OWNER="${IMAGE_OWNER:-bmanojlovic}"
PUSH="${PUSH:-0}"

BASE_HASH=$(cat Dockerfile.base Cargo.toml Cargo.lock crates/*/Cargo.toml $(find vendor -type f | sort) 2>/dev/null \
  | shasum -a 256 | cut -d' ' -f1)
BASE_IMAGE="${REGISTRY}/${IMAGE_OWNER}/obscura-base:${BASE_HASH}"

echo "base image tag: ${BASE_IMAGE}"

if docker image inspect "${BASE_IMAGE}" >/dev/null 2>&1; then
  echo "base image found locally, skipping rebuild"
elif docker pull "${BASE_IMAGE}" >/dev/null 2>&1; then
  echo "base image found in registry, pulled"
else
  echo "base image not found, building"
  docker build -f Dockerfile.base -t "${BASE_IMAGE}" .
  if [ "${PUSH}" = "1" ]; then
    docker push "${BASE_IMAGE}"
  fi
fi

GIT_SHA=$(git rev-parse --short HEAD)
DEV_IMAGE="${REGISTRY}/${IMAGE_OWNER}/obscura:dev-${GIT_SHA}"

echo "building dev image: ${DEV_IMAGE}"
docker build -f Dockerfile.dev \
  --build-arg BASE_IMAGE="${BASE_IMAGE}" \
  --build-arg OBSCURA_VERSION="dev-${GIT_SHA}" \
  -t "${DEV_IMAGE}" .

if [ "${PUSH}" = "1" ]; then
  docker push "${DEV_IMAGE}"
fi

echo "dev image ready: ${DEV_IMAGE}"
