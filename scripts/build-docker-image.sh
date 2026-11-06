#!/bin/bash
# Build and test an opaque OCI archive on a native, unprivileged runner.
# VERSION, SOURCE, CREATED, ARCH and runner paths are supplied by the workflow.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

bash "$script_dir/install-regctl.sh" "$RUNNER_TEMP/tools"
export PATH="$RUNNER_TEMP/tools:$PATH"
cd "$GITHUB_WORKSPACE/source"
python3 scripts/sdk-prepare.py
docker buildx build --platform "linux/$ARCH" --provenance=false --sbom=false --no-cache \
  --build-arg "TUTABRIDGE_VERSION=$VERSION" --build-arg "TUTABRIDGE_REVISION=$SOURCE" \
  --build-arg "TUTABRIDGE_CREATED=$CREATED" \
  --label "org.opencontainers.image.version=$VERSION" \
  --label "org.opencontainers.image.revision=$SOURCE" \
  --label "org.opencontainers.image.created=$CREATED" \
  --label org.opencontainers.image.source=https://github.com/spartanz51/tutabridge \
  --label org.opencontainers.image.licenses=GPL-3.0-or-later \
  --output type=oci,dest=image.tar .
tested=$(sha256sum image.tar | cut -d " " -f1)
regctl image import ocidir://smoke:release image.tar
regctl image export ocidir://smoke:release --name tutabridge:test | docker load
test "$(docker run --rm tutabridge:test --version)" = "tutabridge $VERSION"
# Historical rollback sources predate the Linux fixture stage. Their
# own smoke suite still runs; the strict runner rejects any skipped test.
if [ -f docker/tests/run-real-pass.sh ]; then
  docker buildx build --platform "linux/$ARCH" --provenance=false --sbom=false --no-cache \
    --build-arg "TUTABRIDGE_VERSION=$VERSION" --build-arg "TUTABRIDGE_REVISION=$SOURCE" \
    --build-arg "TUTABRIDGE_CREATED=$CREATED" --target test-binaries \
    --output "type=local,dest=$RUNNER_TEMP/docker-test-binaries" .
  test -x "$RUNNER_TEMP/docker-test-binaries/test-service"
  test -x "$RUNNER_TEMP/docker-test-binaries/test-core"
  docker run --rm --network none --entrypoint /tmp/test-service \
    --mount "type=bind,src=$RUNNER_TEMP/docker-test-binaries/test-service,dst=/tmp/test-service,readonly" \
    tutabridge:test service::lifecycle_tests --nocapture
  docker run --rm --network none --entrypoint bash \
    --mount "type=bind,src=$RUNNER_TEMP/docker-test-binaries/test-core,dst=/tmp/test-core,readonly" \
    --mount "type=bind,src=$GITHUB_WORKSPACE/source/docker/tests/run-real-pass.sh,dst=/tmp/run-real-pass.sh,readonly" \
    tutabridge:test /tmp/run-real-pass.sh
  export TUTABRIDGE_HEALTH_TEST_BINARY="$RUNNER_TEMP/docker-test-binaries/test-service"
fi
TUTABRIDGE_DOCKER_IMAGE=tutabridge:test python3 "$script_dir/docker-smoke.py"
test "$tested" = "$(sha256sum image.tar | cut -d " " -f1)"
echo "sha256=$tested" >> "$GITHUB_OUTPUT"
