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
Extract its ``fixtures`` directory and run:

.. code-block:: bash

  WEINAV_FIXTURES=/path/to/fixtures nix develop -c \
    cargo test --test fixtures -- --ignored

These tests check seed and EXTRA data against captured bytes. They also
check RTCM field encoding and decoding, and rejection of invalid QZSS data.
Normal test runs skip these tests. Use the command above to run them.

Other ignored tests use the reviewed seed, broadcast snapshot and
``missing_vs_huawei.csv`` from the generation 3 review. Set
``WEINAV_REVIEW_FIXTURES`` to that directory and run the same command. These
tests check that the 47 formerly dropped seed records fit inside the
measured envelope without bounds. They also compare a build with Huawei's
output: GPS records agree within 0.15 metre for 2 hours on each side of the
epoch, the BeiDou GEO and QZSS records are the same as Huawei's, and no field
is on its vendor limit unless Huawei's field is.

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
commands. Also measure peak memory for a variant list: each active variant
keeps its own source data. Source loading is serial to limit temporary
memory use. Orbit calculations and variants share one worker pool.

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

NixOS tests
-----------

The NixOS checks evaluate module options and run a VM with controlled source
responses. The VM test checks service permissions, offline builds,
compression, HTTP responses, failed updates, expiry and reboot recovery:

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
coordinate conversion divides by eccentricity. Without a seed, the validity
window of the partial EXTRA file starts at the 2-hour grid step that contains
the build time and ends 75 hours later, so it brackets every shipped epoch.

Each Kepler record is fitted to 49 positions from 2 hours before to 2 hours
after the epoch, every 300 seconds, as Huawei's library does. The fit limit
applies to the standard error, which divides the squared residuals by
``3n - 15``. The builder never bounds or clamps a field. A record with a field
outside its vendor range is removed, and the report lists it. Huawei omits the
same BeiDou GEO and QZSS records. Fields are rounded to nearest, with ties away
from zero. A BeiDou orbit near 42164 km with an inclination below 10 degrees is
a GEO orbit and uses the extra rotation of the BeiDou ICD. BeiDou records use
the GPS gravity constant, as Huawei's records do; broadcast evaluation keeps
the ICD value. Records carry zero ``gamma_n`` and ``af2``, as genuine Huawei
files do. BeiDou prediction clocks have a common offset that drifts against the
seed. The builder removes the median offset against the seed in each epoch, or
against broadcast when there is no seed, and reports it as
``clock_alignment_ns``.

The last broadcast source file is the broadcast health snapshot. Health
screening uses the newest fresh record in this file only, so the external
gate can repeat each decision from the published file. The processor writes
a copy of this file next to the report only when the checks pass. Orbit
screening, AGNSS and Klobuchar data use all broadcast files. The newest
Klobuchar record wins, from a RINEX 3 header or a RINEX 4 ``ION`` record.

The report uses the ``gb-gnss-zipbuilder/report/1`` format. The gate reads
the ``format``, ``flavor``, ``built_at_unix``, ``seed``, ``health``,
``plan`` and ``degraded`` report fields; the other fields are for diagnosis.
``seed`` names the seed version, and ``plan`` names the source for each
constellation and epoch range. The internal source policy is under
``policy``.

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
This rule applies to ZIP files, reports and the discovery manifest,
including sidecars from earlier publications.
The completed set moves to a generation directory on the output filesystem.
A single atomic link replacement publishes the set.

Build ``manifest.json`` from published metadata, not the current instance
settings: an instance can still serve an older build after an update fails.
Keep original timestamps and use URLs for specific generations so checksums
remain valid across updates. Publish the manifest and its sidecars through
their own atomic link. Publish each passed report and its broadcast health
snapshot in the generation directory, because the external gate needs both.
Check the snapshot checksum against the report before publication. Keep the
seed, the other source data and the private report copies private.

Cleanup must keep each current generation and every generation named in the
current manifest. Failed updates and reboots must keep the previous output,
even after its timestamp expires. Cache cleanup must keep objects referenced
by a committed source manifest or current URL metadata.

Dependency updates
------------------

Dependabot checks Cargo, Nix flake inputs and GitHub Actions each Monday at
03:27 UTC. The ``dependencies`` group combines their version updates in one
pull request. The configuration is in ``.github/dependabot.yml``.
Cargo version updates wait two days after release, include indirect
dependencies, and change only ``Cargo.lock``.
External actions must use full commit pins.

Enable Dependabot alerts and security updates in the repository settings.
Cargo security updates use the separate ``cargo-security`` group. They do
not wait for the weekly version update schedule or the two-day cooldown.

The ``merge dependency updates`` workflow runs after the build workflow.
It checks the Dependabot author and each commit signature. It permits
changes to ``Cargo.lock``, ``flake.lock`` and action pins only.
Changes to other files, including ``Cargo.toml``, need manual review.

For ordinary Cargo updates, the merge check reads the publication age from
the base branch's ``.cargo/config.toml``. It checks each new package version
against crates.io, including indirect dependencies. Keep the Cargo cooldown
in ``.github/dependabot.yml`` equal to this age. A signed commit with the
``cargo-security`` group is exempt from the age check. The build, test and
lint checks still apply.

The workflow advances the default branch by fast-forward only. This keeps
the signed Dependabot commits unchanged. Both tested branches must still be
current, and repository rules must permit the update. No pull request code
runs in the merge workflow.

The merge checks and fast-forward use the built-in ``GITHUB_TOKEN``.
The token needs permission to update the default branch by fast-forward.
Require the build and lint checks in the branch rules.

For rebase comments, create a fine-grained personal access token from a
user account with push access to this repository. Select only this
repository and give the token ``Pull requests: Read and write`` permission.
GitHub adds ``Metadata: Read-only`` permission. Save the token as the
``DEPENDABOT_REBASE_TOKEN`` Actions repository secret. Use an Actions
secret, not a Dependabot secret. Replace it before it expires.

If the base branch advances or the checked branch needs a rebase, the
workflow posts ``@dependabot rebase`` and leaves the pull request open.
It uses the personal token only to identify the user and post the comment.
It sends at most one request from that user for each head and base revision
pair. Old requests from ``github-actions[bot]`` do not prevent a new request.
Dependabot must update the branch and the new build must pass before a
merge can occur. If the secret is absent or invalid, a required rebase
fails the workflow. A pull request that needs no rebase can still merge.

If a merge fails for another reason, read the workflow log. If a package is
too new, rerun the failed merge job after the age limit. A failed
publication lookup leaves the pull request open.

To update the lock files locally, run:

.. code-block:: bash

  nix flake update
  nix develop --no-update-lock-file -c cargo update -Z min-publish-age
