# Local container validation

Run from a checkout whose pinned SDK has been prepared with
`python3 scripts/sdk-prepare.py`. No registry credentials or Tuta account are
needed. These commands are also run by `.github/workflows/docker.yml`.

The pure retry policies and status validation can be tested without Docker,
subprocess fixtures or a Tuta account:

```sh
cargo test --locked -p tutabridge --bin tutabridge service::policy::tests
cargo test --locked -p tutabridge --bin tutabridge service::health::tests
```

These tests cover retry delays, repair detection, log cadence and health input
validation. The integration checks below additionally exercise real locks,
process cleanup, TLS and storage.

```sh
docker build -t tutabridge:test .
docker build --target test-binaries --output type=local,dest=/tmp/tutabridge-tests .
docker run --rm --network none --entrypoint /tmp/test-service \
  --mount type=bind,src=/tmp/tutabridge-tests/test-service,dst=/tmp/test-service,readonly \
  tutabridge:test service::lifecycle_tests --nocapture
docker run --rm --network none --entrypoint bash \
  --mount type=bind,src=/tmp/tutabridge-tests/test-core,dst=/tmp/test-core,readonly \
  --mount "type=bind,src=$PWD/docker/tests/run-real-pass.sh,dst=/tmp/run-real-pass.sh,readonly" \
  tutabridge:test /tmp/run-real-pass.sh
TUTABRIDGE_DOCKER_IMAGE=tutabridge:test \
TUTABRIDGE_HEALTH_TEST_BINARY=/tmp/tutabridge-tests/test-service \
  python3 -m unittest discover -s scripts/tests -p test_docker_smoke.py -v
```

The runtime remains the default final Dockerfile stage. The `test-binaries`
stage exports test executables only; they are mounted into disposable containers,
never included in the product image. Rebuild both targets after source changes.
Cargo's Docker cache is locked during each build to avoid concurrent SDK outputs.

The process tests isolate Linux subreaper and signal state in child test processes.
They assert observed lock contention, bounded/cancelled acquisition, group cleanup
before lock release, adopted daemon reaping and retention of Tokio's worker PID.
The GPG smoke additionally runs the supervisor as PID 1 without tini and verifies
that dead agents disappear, not merely that they stop running.

The pass harness creates fresh synthetic keys and a Git store under a private
`/tmp/tutabridge-real-pass-*` directory, removes both on exit and never needs
network access. It checks original ciphertext preservation on encryption and
read-back failures, atomic replacement, interrupted staging cleanup and Git
history that excludes temporary names and unrelated staged files.

The health fixture runs the real IMAP/SMTP servers with a backend that panics on
any account access, a pinned certificate without a localhost SAN and an explicit
loopback address. The public health command must pass, then fail when SMTP stops.
Without `TUTABRIDGE_HEALTH_TEST_BINARY` this one smoke is skipped; CI always sets it.
HTTP rejection scenarios use synthetic credentials against a local fixture.
They do not prove successful Tuta authentication, TOTP or behavior on a real NAS.
