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

Cargo and Nix use the release profile in ``Cargo.toml``. The standard-library
``optimize_for_size`` feature does not override this profile. Keep ``-lgcc``
in the Nix link flags: mimalloc's C code needs GCC integer runtime helpers.

Normal Rust tests use synthetic data and local servers. They do not need
access to source providers.

Cargo forbids unsafe Rust code in all targets of this package, including
tests. An ``allow`` attribute cannot override this rule.

Captured data tests
-------------------

Some tests need the captured files from the supplied specification archive.
Extract its ``fixtures`` directory and run::

  WEINAV_FIXTURES=/path/to/fixtures nix develop -c \
    cargo test --test fixtures -- --ignored

These tests check seed and EXTRA data against captured bytes. They also
check RTCM field encoding and decoding, and rejection of invalid QZSS data.
Normal test runs skip these tests. Use the command above to run them.

Processing performance
----------------------

Use one fixed source cache and one fixed ``--at`` value for a comparison.
Do not fetch data between runs. Select a time covered by the cached sources.
Check the report status and satellite counts before you measure a build.
A quick refusal does not measure a complete build. Keep each executable and
its output in a separate path. Stop other builds before you measure time.

Before a change, copy the release executable::

  mkdir -p target/performance
  nix develop -c cargo build --release
  cp target/release/weinav-forge target/performance/before

After the change, build again and copy it to ``target/performance/after``.
Set ``BENCH_CACHE`` to the source cache path. Set ``BENCH_AT`` to the fixed
RFC 3339 time. Export both variables, then run::

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
commands. Also measure peak memory for a variant list: each active variant
keeps its own source data. Source loading is serial to limit temporary
memory use. Orbit calculations and variants share one worker pool.

The batch comparison test needs a cache with Huawei and Huawei-plus sources.
The three-system Huawei-plus build must pass its checks at the selected
time. Run it with::

  WEINAV_PROCESS_CACHE="$BENCH_CACHE" WEINAV_PROCESS_AT="$BENCH_AT" \
    nix develop -c cargo test --release --test cli \
    parallel_batches_preserve_payloads_reports_and_partial_success -- --ignored

Use the profiling build to keep symbols for samply::

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
    prettier --check .github/dependabot.yml .github/workflows/*.yml
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

The HTTP client is wreq. The Huawei profile starts from the wreq-util
OkHttp 3.14 profile. Its reference is the patched NetworkKit OkHttp 3.14.9
client in Huawei Health 16.1.6.320, described in the supplied ``CLIENT.md``.
Some header bindings are inferred. Android TLS details and the active
HMS Core version are unknown, so the profile is an approximation.

Keep the profile in ``src/http.rs`` specific to Huawei hosts. Each redirect
must select the profile for its destination and each request must have a
fresh request ID. Keep certificate checks enabled.

Local server tests check the actual TLS ClientHello, HTTP/2 frames, and
HTTP/1 header case and order. Other tests check fresh request IDs, header
selection after redirects, and separate HTTP and AGNSS gzip layers. Run
these tests with ``nix develop -c cargo test http::tests``.

Source cache and processing
---------------------------

The cache contains immutable source objects, URL metadata, and one JSON
manifest per flavor. A complete fetch replaces the manifest atomically.
Processing checks the content hash of each cached object. The report and ZIP
are each written through a temporary file.

The processor checks the data format, satellite counts, time coverage and
source policy before packing. ANTEX corrections use the satellite's radial
phase-centre offset; transverse antenna offsets are not modelled. The
quantised orbit check permits 0.5 metre beyond the configured fit limit.
Nearly circular orbits use numerical derivatives because the analytic
coordinate conversion divides by eccentricity.

Service publication
-------------------

The fetch service gets the combined source data needed by enabled instances.
The build service has no network access and reads the cache through a
read-only mount. The services use separate dynamic users. A shared lock
prevents fetching, building and cleanup from running at the same time.

Tmpfiles manages shared storage independently of the dynamic users. Fixed
groups and default access control lists give each service the required
access. Keep the top directories and lock file owned by root. Workers must
not be able to replace the lock or change top-directory permissions. These
restrictions prevent access by a later account with a reused UID. Nginx must
not be able to write published files or read private cache files and reports.
The VM test checks these restrictions after forced UID changes.

Each build first writes private staging files. It then makes gzip and Brotli
sidecars with ``pigz`` and ``brotli``, and checks both by decompression.
It keeps a sidecar only if its size is less than the original file size.
This rule applies to ZIP files and the discovery manifest, including
sidecars from earlier publications.
The completed set moves to a generation directory on the output filesystem.
A single atomic link replacement publishes the set.

Build ``manifest.json`` from published metadata, not the current instance
settings: an instance can still serve an older build after an update fails.
Keep original timestamps and use URLs for specific generations so checksums
remain valid across updates. Publish the manifest and its sidecars through
their own atomic link. Keep source data, reports and build metadata private.

Cleanup must keep each current generation and every generation named in the
current manifest. Failed updates and reboots must keep the previous output,
even after its timestamp expires. Cache cleanup must keep objects referenced
by a committed source manifest or current URL metadata.

Lock file updates
-----------------

The ``update lock files`` workflow runs weekly. You can also start it
manually on the default branch. Cargo uses the minimum publication age in
``.cargo/config.toml``: two days, with the ``deny`` policy.

To update the lock files locally, run::

  nix flake update
  nix develop --no-update-lock-file -c cargo update -Z min-publish-age

Automatic updates open a pull request with only the two lock files. The
``lock-file-update`` status covers build, test and lint checks on both
supported systems. The workflow merges the checked commit by fast-forward.
If checks fail, either branch changes, or repository rules block the merge,
the pull request stays open.

Permit GitHub Actions to create pull requests in the repository settings.
The ``GITHUB_TOKEN`` must also be able to update the default branch reference
by fast-forward.

Read the failed job's log before you retry an update. A new run creates a
new update branch and pull request. Previous open pull requests stay open.

GitHub Action updates
---------------------

External actions use full commit pins. Dependabot checks for action updates
each Monday at 03:27 UTC and groups them in one pull request. The schedule
is in ``.github/dependabot.yml``. Cargo and Nix lock files use the separate
weekly workflow above, which keeps Cargo's minimum publication age.

The normal pull request checks test the new action pins. After a successful
build, ``merge action updates`` checks that Dependabot opened the pull
request and that only action pins changed. It permits a fast-forward only
when the tested head and base are still current and repository rules permit
the merge. Otherwise, the pull request stays open. The merge workflow does
not check out or execute code from the pull request.

This setup uses Dependabot and the built-in ``GITHUB_TOKEN``. No personal
access token or extra repository secret is required. The token must have
permission to update the default branch by fast-forward. CI rejects
external actions that do not use full commit pins.
