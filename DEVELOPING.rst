Development
===========

Build and check
---------------

Use the Nix development shell for the pinned Rust toolchain::

  nix develop -c cargo build --release
  nix develop -c cargo test
  nix develop -c cargo fmt --check
  nix develop -c cargo clippy --all-targets -- -D warnings
  nix flake check

Use ``nix develop -c cargo fmt`` to format Rust code. Use ``nix build`` to
build the static executable. The CI workflow checks ``x86_64-linux`` and
``aarch64-linux`` on separate runners. A local flake check builds the checks
for the host system.

Normal Rust tests use synthetic data and local servers. They do not need
access to source providers.

Captured data tests
-------------------

Some tests need the captured files from the supplied specification archive.
Extract its ``fixtures`` directory and run::

  WEINAV_FIXTURES=/path/to/fixtures nix develop -c \
    cargo test --test fixtures -- --ignored

These tests check seed and EXTRA data against captured bytes. They also
check RTCM field encoding and decoding, and rejection of invalid QZSS data.
Normal test runs skip these tests. Use the command above to run them.

NixOS tests
-----------

The NixOS checks evaluate module options and run a VM with controlled source
responses. The VM test checks service permissions, offline builds,
compression, HTTP responses, failed updates, expiry and reboot recovery::

  nix build .#checks.x86_64-linux.module-evaluation
  nix build .#checks.x86_64-linux.module-vm

Use ``aarch64-linux`` in these commands on an AArch64 host. These checks also
run through ``nix flake check``.

Format and lint
---------------

Get the other check tools from nixpkgs. Use the flake inputs to select the
same tool versions as CI::

  nix shell --inputs-from . nixpkgs#nixfmt-rfc-style -c \
    nixfmt --check flake.nix nix/module.nix nix/tests/*.nix
  nix shell --inputs-from . nixpkgs#statix -c statix check
  nix shell --inputs-from . nixpkgs#deadnix -c \
    deadnix --fail flake.nix nix/module.nix nix/tests/*.nix
  nix shell --inputs-from . nixpkgs#taplo -c \
    taplo fmt --check Cargo.toml .cargo/config.toml
  nix shell --inputs-from . nixpkgs#prettier -c \
    prettier --check .github/workflows/*.yml
  nix shell --inputs-from . nixpkgs#actionlint -c \
    actionlint .github/workflows/*.yml
  nix shell --inputs-from . nixpkgs#python3Packages.docutils -c \
    rst2pseudoxml --exit-status=1 README.rst
  nix shell --inputs-from . nixpkgs#python3Packages.docutils -c \
    rst2pseudoxml --exit-status=1 DEVELOPING.rst

Both RST files use ASD-STE100 Simplified Technical English and ASCII
characters. Keep text lines at 79 characters or less. A table line can be
longer. There is no RST formatter; docutils checks the RST syntax.
Keep program use and service operation in ``README.rst``. Keep development
and repository maintenance in this file.

Huawei HTTP profile
-------------------

The Huawei profile starts from the wreq-util OkHttp 3.14 profile. It uses
the patched NetworkKit client described in the supplied ``CLIENT.md`` as
its reference. Some header bindings are inferred. Android TLS details and
the active HMS Core version are unknown, so the profile is an approximation.

TLS offers the five permitted TLS 1.2 cipher suites and the three selected
TLS 1.3 cipher suites. ALPN offers HTTP/2 before HTTP/1.1. Session tickets
and session reuse are enabled. HTTP/2 uses 16 MiB stream and connection
windows, one initial setting, stream ID 3, and the recorded pseudo-header
order. It sends no priority frames.

Local server tests check the actual TLS ClientHello, HTTP/2 frames, and
HTTP/1 header case and order. Other tests check fresh request IDs, header
selection after redirects, and separate HTTP and AGNSS gzip layers.
Keep certificate checks enabled. The client uses WebPKI roots.

Lock file updates
-----------------

The GitHub Actions workflow ``update lock files`` runs each Monday at
03:27 UTC. You can also start it manually on the default branch. It updates
``flake.lock`` and ``Cargo.lock``. Cargo uses the minimum publication age in
``.cargo/config.toml``, currently two days, with the ``deny`` policy.

To update the lock files locally, run::

  nix flake update
  nix develop --no-update-lock-file -c cargo update -Z min-publish-age

Each automatic update opens a pull request with only the two lock files.
The workflow runs the shared build, test and lint checks on both supported
systems. The ``lock-file-update`` status reports their result on the pull
request. Hash checks verify that the checks did not change the lock files.

After the checks pass, the workflow merges the pull request by fast-forward.
It keeps the checked commit and its dates. It does not rebase, squash or make
a merge commit. If a check fails, either branch changes, or a repository rule
blocks the merge, the pull request stays open.

Permit GitHub Actions to create pull requests in the repository settings.
The ``GITHUB_TOKEN`` must also be able to update the default branch reference
by fast-forward. The workflow uses the Git references API with ``force`` set
to ``false`` for this step. GitHub then marks the pull request as merged.
No personal access token is required.

Read the failed job's log before you retry an update. A new run creates a
new update branch and pull request. Previous open pull requests stay open.
