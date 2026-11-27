# Contributing to BroScribe

Open an issue with the BroScribe version, OS and architecture, the steps to
reproduce the behavior, and what you expected. Use synthetic audio and invented
names in reports, screenshots and tests. Follow [SECURITY.md](SECURITY.md)
for vulnerabilities.

## Build and test

Install rustup and let `rust-toolchain.toml` select Rust 1.100.0. Use the
[native prerequisites](docs/install.md) for your platform. Fetch the pinned
sherpa-onnx libraries before building.

On Apple silicon:

```sh
third_party/sherpa-onnx/fetch.sh --target osx-arm64
cargo fmt --all --check
scripts/build-target.sh aarch64-apple-darwin clippy --locked --workspace --all-targets -- -D warnings
scripts/build-target.sh aarch64-apple-darwin test --locked --workspace
```

On Linux, run `scripts/docker-linux-test.sh`, or
`scripts/docker-linux-test.sh --amd64` for the Ubuntu 24.04 x64 baseline. On
Windows, use the MSVC environment and commands in the installation guide, and
run `crates/broscribe-platform-windows/tests/cleanup-matcher.Tests.ps1` under
both Windows PowerShell 5.1 and PowerShell 7. Run shipping script tests
with `python3 -m unittest discover -s scripts/tests`. When changing
`Cargo.lock`, also run `scripts/check-publish-age.py Cargo.lock`. Downloaded
models are necessary only for tests that explicitly require them.

## Screenshots

The README screenshots come from invented data in a Linux XFCE desktop in
Docker. To see them locally, run:

```sh
scripts/linux-desktop/screenshots.sh /tmp/broscribe-shots
```

It needs Docker with linux/arm64 (native on Apple silicon and arm64 Linux) and
writes the four images and their manifest. A pull request that changes the UI
gets the same images as a `screenshots` artifact of its workflow run; `main`
publishes them to the `screenshots` branch. See
[scripts/linux-desktop/README.md](scripts/linux-desktop/README.md).

## Submit a pull request

Keep each PR focused. Summarize the problem, the change and the tests you ran.
Follow the [repository rules](AGENTS.md) and use Conventional Commit messages.

CI must pass for every PR into `main`. PRs authored by
`@raphaelbarreiros` or `@Renan-Brasilio` from a branch in this repository can
merge after CI passes.
Other authors need a current approval from one of those maintainers. A new head
commit needs a new review. Maintainers merge through the workflow described in
the [release checklist](docs/release.md#merge-policy).
