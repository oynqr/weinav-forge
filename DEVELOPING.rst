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

The first profile on 2026-09-25 used all five constellations. About 87 percent
of CPU samples were in trigonometry, orbit position and fit residuals.
The work was ranked in this order:

1. Replace numerical fit derivatives with analytic derivatives. Numerical
   derivatives needed two orbit evaluations per free parameter and sample.
2. Measure release optimization level 3 against level ``z``.
3. Reduce matrix allocation and source parsing. These costs were much smaller
   in the first profile, so these changes were deferred.

The fitter now evaluates an analytic Jacobian once per sample. It converts
the derivatives to the same regular eccentricity coordinates as the fit.
Below an eccentricity of ``1e-6``, it keeps central differences to avoid
division by a small eccentricity. Tests compare each analytic column with
central differences for all four Kepler constellations. Other tests cover
circular orbits and the fixed QZSS mean-motion parameter.

The orbit model, iteration limits, fit limits and output checks did not
change. Different derivatives can change fitted parameters and encoded
bytes. Compare decoded results and gate reports, not only ZIP checksums.
Archive UUIDs also change between runs. Use the report's payload checksums
to compare file bytes without those UUIDs.

The comparison used commit ``a1f2651`` as the baseline, hyperfine 1.20.0,
samply 0.13.1 and Rust 1.100.0-nightly (2026-08-21). A KVM guest on an AMD
Ryzen 9 7950X3D used CPU 0. Each timing had one warm-up and five measured
runs. The fixed time was ``2026-09-25T11:40:00Z``. The cache manifest had this
SHA-256 checksum::

  6b41436dfb850190f40baa1845d32050b6402f45d625a2e5a7a76bc82d4e19b4

For ``huawei-plus`` with GPS, Galileo and QZSS, without AGNSS, native release
times were as follows. Each value is the mean and standard deviation.

============================ ==================
Build                        Time in seconds
============================ ==================
Baseline                     10.045 +/- 0.035
Analytic Jacobian, level z    2.239 +/- 0.007
Analytic Jacobian, level 3    1.683 +/- 0.011
============================ ==================

All three builds passed the output checks and kept the same 2,141 records.
The two analytic builds had identical payload checksums. Optimization level
3 increased the native executable from 3,778,760 to 4,919,608 bytes.

For the same passing workload, the static Nix executable changed from
10.612 +/- 0.013 seconds to 2.303 +/- 0.015 seconds, or 4.61 times faster.
Its size changed from 3,431,384 to 4,647,896 bytes. Both versions kept the
same 2,141 records. The largest RMS error after encoding changed from
0.095 metre to 0.084 metre, below the unchanged 1.5 metre limit.

The full five-constellation workload kept the same 6,936 records, excluded
satellites and gate results. Both versions refused that source snapshot
because GLONASS and BDS had too few satellites. This refusal workload was
kept separate from the passing ZIP build. Its static build time changed
from 15.993 +/- 0.107 seconds to 3.321 +/- 0.005 seconds, or 4.82 times
faster. The timing command required exit code 4 on each run.

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

The HTTP client is wreq. The Huawei profile starts from the wreq-util
OkHttp 3.14 profile. Its reference is the patched NetworkKit OkHttp 3.14.9
client in Huawei Health 16.1.6.320, described in the supplied ``CLIENT.md``.
Some header bindings are inferred. Android TLS details and the active
HMS Core version are unknown, so the profile is an approximation.

Huawei configuration, download and AGNSS hosts receive the application
headers and a fresh random request ID. Other hosts receive the weinav-forge
User-Agent. Each redirect selects the profile for its destination. Time
limits are 8 seconds for a connection and 30 seconds for a complete download,
including redirects.

TLS offers the five permitted TLS 1.2 cipher suites and the three selected
TLS 1.3 cipher suites. ALPN offers HTTP/2 before HTTP/1.1. Session tickets
and session reuse are enabled. HTTP/2 uses 16 MiB stream and connection
windows, one initial setting, stream ID 3, and the recorded pseudo-header
order. It sends no priority frames.

Local server tests check the actual TLS ClientHello, HTTP/2 frames, and
HTTP/1 header case and order. Other tests check fresh request IDs, header
selection after redirects, and separate HTTP and AGNSS gzip layers.
Keep certificate checks enabled. The client uses WebPKI roots.

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

Service publication
-------------------

The fetch service gets the combined source data needed by enabled instances.
The build service has no network access and reads the cache through a
read-only mount. The services use separate dynamic users. A shared lock
prevents fetching, building and cleanup from running at the same time.

Systemd assigns the ``weinav-fetch`` and ``weinav-build`` users for each run.
These names differ from the old static user names, so an old account cannot
prevent dynamic user allocation after an upgrade. Fixed groups control
access to stored files. The fetch service uses ``weinav-forge-cache``. The
build service uses ``weinav-forge-work`` and can read the cache. Nginx uses
``weinav-forge-public`` to read published files, but cannot write them.

Tmpfiles manages these shared directories, including custom paths. Their
top directories belong to root and have no access for other users. The
services cannot change those permissions. Thus a later user with a reused
UID cannot access files left by an earlier run. Set-group-ID directories
and default access control lists keep group access on new files. Tmpfiles
also updates ownership and permissions on existing data during an upgrade.
The filesystem must support POSIX access control lists.

The shared directory lifetime is independent of either service. The module
therefore uses tmpfiles instead of private ``StateDirectory`` and
``CacheDirectory`` directories, which also limit access from other services.
The root-owned lock file remains in place between runs. Neither worker can
replace it. The services cannot execute files from the data directories.

The VM test forces new UIDs by reserving the old UIDs for other accounts.
It checks that the new workers can use the stored data and that the accounts
with reused UIDs cannot read it. It also checks that nginx cannot write to
the published directory or read cache files and reports.

Each build first writes private staging files. It then makes gzip and Brotli
sidecars with ``pigz`` and ``brotli``, and checks both by decompression.
It keeps a sidecar only if its size is less than the original file size.
An equal or larger sidecar is deleted. Each run also removes such sidecars
from earlier publications. Nginx uses the original file when a requested
sidecar is absent. Both ZIP files and the discovery manifest use this rule.
The completed set moves to a generation directory on the output filesystem.
A single atomic link replacement publishes the set.

Nginx selects the identity, gzip or Brotli file from one immutable generation
for each request. Each response has a no-store cache policy. Source files,
reports, and directory listings have no public URL.

After the instance builds, the service creates ``manifest.json`` from the
published files. It records their SHA-256 checksums, byte sizes and original
timestamps. Each entry uses a URL for a specific generation. Thus its
checksum remains valid when a later build updates the instance's latest URL.
Only the ZIP file has a public generation URL; its build metadata stays
private. Metadata stored with each ZIP describes the actual build, including
when a later build with different settings fails.

The manifest and its sidecars have their own atomic publication link in
``.manifest``. Links in the output root provide access to these files.
A failed manifest update keeps the previous set. Cleanup keeps all archive
generations named in the current manifest, as well as each instance's
current generation. Unlisted old generations use the normal retention
limits. An instance with no published ZIP has no manifest entry.

The module's ``compression.threads`` option sets the pigz thread count; the
default is 1. Brotli uses one thread. ``compression.gzip.level`` and
``compression.brotli.quality`` both default to 6. Each format also has an
``enable`` and a ``package`` option.

Failed updates and reboots keep the previous published generation,
including output whose timestamp has expired. Cleanup always keeps the
current generation. Cache cleanup removes old URL metadata and old objects
that no committed manifest or current URL metadata references.

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
