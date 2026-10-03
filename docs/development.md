# Development environment

This document describes the reproducible local development environment for
the Wotoha workspace. It is based on the repository requirements at the
current `main` baseline (`dd3dba627d8f778352b1ee6abd40839e7131713e`) and does
not change production behavior.

## Required toolchain

- Linux x86_64 is the tested host platform.
- Rust is pinned by [`rust-toolchain.toml`](../rust-toolchain.toml) to
  `1.95.0`, with `rustfmt`, `clippy`, and the
  `x86_64-unknown-linux-musl` target.
- A C compiler and linker are required. GCC/build-essential is sufficient;
  Clang is not required by this repository.
- `cmake` and `pkg-config` are required by the `libopus_sys` native build
  path. The Docker builder installs these explicitly.
- `ninja-build` is required for the repository's release packaging path, which
  selects the Ninja CMake generator. It is not needed by the normal CI job.
- Python 3 is required for the CI workspace-license metadata check.
- Docker Engine, Buildx, and Compose are required only for container parity
  checks.

The application uses Rustls rather than an OpenSSL-linked reqwest backend,
Symphonia for audio decoding, and RTEN with embedded model assets. No separate
FFmpeg, ALSA, PulseAudio, OpenSSL development package, or system ONNX Runtime
installation is required for the workspace build.

## Ubuntu/Debian setup

Install the host packages after confirming that the package manager and sudo
policy are available:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends \
  build-essential cmake pkg-config ninja-build python3 git curl wget \
  unzip tar jq ca-certificates
```

Install or activate the pinned Rust toolchain:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
rustup toolchain install 1.95.0 \
  --profile minimal \
  --component clippy,rustfmt \
  --target x86_64-unknown-linux-musl
```

The repository's `rust-toolchain.toml` selects this toolchain automatically
when commands are run from the repository.

## Workspace verification

Run the same checks as CI, plus the analysis-lab help/build check:

```sh
cargo metadata --locked --no-deps
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked --no-deps -- -D warnings

bash -n deploy/*.sh deploy/tests/*.sh
bash deploy/tests/run.sh
cargo run --locked -p wotoha-analysis-lab -- --help
```

The analysis lab provides research commands including `research-pass` and
`research-tempo-advisor`. Research outputs and corpus data belong outside the
repository; do not commit them or `.wotoha-analysis/`.

## Container parity

Build locally without pushing to a registry:

```sh
docker buildx build \
  --platform linux/amd64 \
  --load \
  --build-arg SOURCE_COMMIT="$(git rev-parse HEAD)" \
  --build-arg IMAGE_VERSION=local \
  --tag wotoha-local:dev .
```

The container workflow's smoke checks should be run against the local tag:

```sh
docker run --rm --entrypoint id wotoha-local:dev -u
docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,exec,mode=1777 \
  --mount type=tmpfs,destination=/data,tmpfs-mode=0777 \
  --cap-drop=ALL \
  --security-opt=no-new-privileges:true \
  wotoha-local:dev --self-check
```

The expected container user is UID `10001`. The image must not require a
writable root filesystem and must keep application data under `/data`.

## Namespace and bubblewrap note

Wotoha does not depend on bubblewrap. It does not invoke `bwrap`, create a
network namespace, or require `CAP_NET_ADMIN`. If a development runner emits
an error such as `bwrap: loopback: Failed RTM_NEWADDR: Operation not
permitted`, diagnose that runner's namespace policy separately. Do not solve
it by enabling privileged containers, disabling seccomp/AppArmor, or adding
host capabilities.

## Research and data hygiene

Analysis-lab commands are deterministic research tooling, not production
runtime changes. Keep generated fixtures, WAVs, reports, and temporary Cargo
build directories outside the Git worktree. External analyzer observations are
inputs for comparison only; they are not Wotoha ground truth.
