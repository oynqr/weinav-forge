Development
===========

Build and check
---------------

Use the Nix development shell for the pinned Rust toolchain:

.. code-block:: bash

  nix develop -c cargo build --release
  nix develop -c cargo test
  nix develop -c cargo fmt --check
  nix develop -c cargo clippy --all-targets -- -D warnings
  nix flake check

Use ``nix develop -c cargo fmt`` to format Rust code. Use ``nix build`` to
build the static executable. The CI workflow checks ``x86_64-linux`` and
``aarch64-linux`` on separate runners. A local flake check builds the checks
for the host system.

Cargo and Nix use the release profile in ``Cargo.toml``. Keep ``-lgcc`` in
the Nix link flags: mimalloc's C code needs GCC integer runtime helpers.

Normal Rust tests use synthetic data and local servers. They do not need
network access.

Cargo forbids unsafe Rust code in all targets of this package, including
tests. An ``allow`` attribute cannot override this rule.

Captured data tests
-------------------

Some tests need the captured files from the supplied specification archive.
Extract its ``fixtures`` directory and run:

.. code-block:: bash

  WEINAV_FIXTURES=/path/to/fixtures nix develop -c \
    cargo test --test fixtures -- --ignored

Other ignored tests need the reviewed seed, broadcast snapshot and
``missing_vs_huawei.csv`` of the generation 3 review. Set
``WEINAV_REVIEW_FIXTURES`` to that directory and run the same command. Normal
test runs skip all of these tests.

Field envelopes
---------------

``src/envelopes.rs`` is made from the external gate's ``g2_envelopes.json``.
Before each build, set ``WEINAV_GATE_ENVELOPES`` to that file and run:

.. code-block:: bash

  WEINAV_GATE_ENVELOPES=/path/to/g2_envelopes.json nix develop -c \
    cargo test --test fixtures -- --ignored --exact envelopes_match_the_gate

When the test fails, it writes the new table to ``target/tmp/envelopes.rs``.
Copy that file to ``src/envelopes.rs`` and run ``cargo fmt``. Do not change a
limit by hand.

Processing performance
----------------------

Use one fixed source cache and one fixed ``--at`` value for a comparison.
Do not fetch data between runs. Select a time covered by the cached sources.
Check the report status and satellite counts before you measure a build.
A quick refusal does not measure a complete build. Keep each executable and
its output in a separate path. Stop other builds before you measure time.

Before a change, copy the release executable:

.. code-block:: bash

  mkdir -p target/performance
  nix develop -c cargo build --release
  cp target/release/weinav-forge target/performance/before

After the change, build again and copy it to ``target/performance/after``.
Set ``BENCH_CACHE`` to the source cache path. Set ``BENCH_AT`` to the fixed
RFC 3339 time. Export both variables, then run:

.. code-block:: bash

  nix shell --inputs-from . nixpkgs#hyperfine nixpkgs#util-linux -c \
    hyperfine --warmup 1 --runs 5 \
    --export-json target/performance/timings.json \
    --parameter-list binary before,after \
    'taskset -c 0 target/performance/{binary} process \
    --flavor huawei-plus --systems gps,galileo,qzs --no-agnss \
    --cache "$BENCH_CACHE" --at "$BENCH_AT" \
    --output target/performance/{binary}-output'

Select an available CPU instead of CPU 0 if necessary. For a service build,
save the executable from ``nix build`` and repeat the same comparison.
This checks the static executable with its own math library.

Set ``--threads 1`` for a single-thread comparison. For parallel builds,
select enough CPUs with ``taskset`` and use the same thread limit in both
commands. Also measure peak memory for a variant list.

The batch comparison test needs a cache with Huawei and Huawei-plus sources.
The three-system Huawei-plus build must pass its checks at the selected
time. Run it with:

.. code-block:: bash

  WEINAV_PROCESS_CACHE="$BENCH_CACHE" WEINAV_PROCESS_AT="$BENCH_AT" \
    nix develop -c cargo test --release --test cli \
    parallel_batches_preserve_payloads_reports_and_partial_success -- --ignored

Use the profiling build to keep symbols for samply:

.. code-block:: bash

  nix develop -c cargo build --profile profiling
  nix shell --inputs-from . nixpkgs#samply nixpkgs#util-linux -c \
    taskset -c 0 samply record --save-only --unstable-presymbolicate \
    --output target/performance/profile.json.gz \
    target/profiling/weinav-forge process \
    --flavor huawei-plus --systems gps,galileo,qzs --no-agnss \
    --cache "$BENCH_CACHE" --at "$BENCH_AT" \
    --output target/performance/profile-output

The host must permit access to Linux performance events. A single CPU also
limits the memory needed for the recording. Keep the profile and its symbol
file together. Time the release executable without the profiler attached.

Compare payload checksums in the reports: archive UUIDs change between runs.
For changes to orbit fitting, also compare satellite counts, RMS errors and
gate results. Test nearly circular orbits, fixed QZSS mean motion and each
available SIMD level. Keep acceptance limits unchanged during optimization.
Measure peak resident memory when you change the allocator.

HTTP tests
----------

The HTTP tests use local servers. Run them with:

.. code-block:: bash

  nix develop -c cargo test http::tests

NixOS tests
-----------

The NixOS checks evaluate module options and run a VM with controlled source
responses:

.. code-block:: bash

  nix build .#checks.x86_64-linux.module-evaluation
  nix build .#checks.x86_64-linux.module-vm

Use ``aarch64-linux`` in these commands on an AArch64 host. These checks also
run through ``nix flake check``.

Format and lint
---------------

Get the other check tools from nixpkgs. Use the flake inputs to select the
same tool versions as CI:

.. code-block:: bash

  nix shell --inputs-from . nixpkgs#nixfmt -c \
    nixfmt --check flake.nix nix/module.nix nix/tests/*.nix
  nix shell --inputs-from . nixpkgs#statix -c statix check
  nix shell --inputs-from . nixpkgs#deadnix -c \
    deadnix --fail flake.nix nix/module.nix nix/tests/*.nix
  nix shell --inputs-from . nixpkgs#taplo -c \
    taplo fmt --check Cargo.toml .cargo/config.toml
  nix shell --inputs-from . nixpkgs#prettier -c \
    prettier --check .github/dependabot.yml .github/workflows/*.yml
  nix shell --inputs-from . nixpkgs#actionlint -c \
    actionlint .github/workflows/*.yml
  nix shell --inputs-from . nixpkgs#python3Packages.docutils -c \
    rst2pseudoxml --syntax-highlight=none --exit-status=1 README.rst
  nix shell --inputs-from . nixpkgs#python3Packages.docutils -c \
    rst2pseudoxml --syntax-highlight=none --exit-status=1 DEVELOPING.rst

Both RST files use ASD-STE100 Simplified Technical English and ASCII
characters. Keep text lines at 79 characters or less. A table line can be
longer. There is no RST formatter; docutils checks the RST syntax.
Keep program use and service operation in ``README.rst``. Keep development
and repository maintenance in this file.

Mark Nix examples with ``.. code-block:: nix`` and JSON examples with
``.. code-block:: json``. The ``documentation`` flake check checks RST
syntax, runs nixfmt and statix on Nix examples, and checks JSON formatting
with Prettier. CI runs this check. Run it locally with:

.. code-block:: bash

  nix build .#checks.x86_64-linux.documentation

Dependency updates
------------------

The Dependabot configuration is in ``.github/dependabot.yml``. Keep its Cargo
cooldown equal to the publication age in ``.cargo/config.toml``. External
actions must use full commit pins.

Enable Dependabot alerts and security updates in the repository settings.
Give the built-in ``GITHUB_TOKEN`` permission to update the default branch by
fast-forward. Require the build and lint checks in the branch rules.

For rebase comments, create a fine-grained personal access token from a
user account with push access to this repository. Select only this
repository and give the token ``Pull requests: Read and write`` permission.
GitHub adds ``Metadata: Read-only`` permission. Save the token as the
``DEPENDABOT_REBASE_TOKEN`` Actions repository secret. Use an Actions
secret, not a Dependabot secret. Replace it before it expires.

If a merge fails, read the workflow log. If a package is too new, rerun the
failed merge job after the age limit.

To update the lock files locally, run:

.. code-block:: bash

  nix flake update
  nix develop --no-update-lock-file -c cargo update -Z min-publish-age
