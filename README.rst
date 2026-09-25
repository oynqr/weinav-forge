weinav-forge
============

Build GNSS assistance data for Huawei watches and Gadgetbridge.
One Rust executable has two commands:

* ``fetch`` gets source files and saves a cache manifest.
* ``process`` reads local files, builds the products, checks the results, and
  writes ``ephemeris.zip`` and ``report.json``. It does not use the network.

Build
-----

Use Nix to build a static Linux executable::

  nix build
  ./result/bin/weinav-forge --help

The flake supports ``x86_64-linux`` and ``aarch64-linux``.

HTTP requests
-------------

The HTTP client is wreq. On the Huawei configuration, download and AGNSS
hosts, it uses a profile based on the patched OkHttp 3.14.9 client in Huawei
Health 16.1.6.320. It sends the application headers and a new random request
ID for each request. It preserves HTTP/1 header case and order.

The profile uses TLS 1.2 and 1.3. It offers HTTP/2 before HTTP/1.1 and supports
gzip responses. It approximates the Huawei client; the Android version and
installed HMS Core version can change that client's behavior. Certificate
checks use WebPKI roots.

Other hosts use the weinav-forge User-Agent. Each redirect selects the profile
for its destination. Request time limits remain 8 seconds for a connection
and 30 seconds for a complete download, including redirects.

Source policy
-------------

Both commands require an explicit flavor. Use ``--plan`` to see its source
policy without reading or downloading source files::

  weinav-forge fetch --flavor huawei-plus --plan

``huawei``
  Use Huawei seed orbits, clocks and EXTRA data. Keep Huawei AGNSS bytes.

``huawei-plus``
  Use open GPS, Galileo and GLONASS orbits. Keep the Huawei GLONASS clock,
  QZSS data, EXTRA data and AGNSS bytes. Use open BeiDou predictions until
  their coverage ends, then use the seed.

``open-plus``
  Use open orbits and clocks. Use the seed for EXTRA and for whole BeiDou
  and QZSS epochs beyond the open prediction coverage. Build AGNSS from
  broadcast navigation data.

``open``
  Use open sources only. BeiDou and QZSS epochs beyond prediction coverage
  are empty. EXTRA has open GPS and Galileo almanacs, GPS ionosphere data,
  and GLONASS frequency data. Other EXTRA regions are absent. This flavor
  requires ``process --allow-degraded``.

The report lists source hashes, provider choices, empty epochs, satellite
removals, fit residuals, and each check result. All flavors use fresh broadcast
health and position checks. A flavor can fail its checks when current sources
have too few usable satellites. A source substitution does not disable checks.

Fetch and process
-----------------

For example::

  weinav-forge fetch --flavor huawei-plus --cache ./cache
  weinav-forge process --flavor huawei-plus --cache ./cache --output ./staging

The cache contains immutable source objects, URL metadata, and one JSON
manifest per flavor. A complete fetch replaces the manifest atomically.
Processing checks the content hash of each cached object. The report and ZIP
are each written through a temporary file.

Select constellations with ``--systems gps,galileo``. GPS is required.
Use ``--no-agnss`` to omit AGNSS. Use the same selection for fetching and
processing, or fetch a superset of the sources needed for processing.

Use ``fetch --offline`` to use cached source files. Use ``--source ROLE=PATH``
to supply a local file. Repeat this option to merge SP3 or RINEX files.
The local files replace cached files for that role. Roles are:

* ``seed``, ``agnss``
* ``prediction``, ``bds-prediction``, ``qzs-prediction``
* ``broadcast``, ``antex``
* ``gps-almanac``, ``galileo-almanac``

For example::

  weinav-forge process --flavor huawei --systems gps --no-agnss \
    --source seed=HiEE_V2.dat \
    --source broadcast=previous.rnx.gz --source broadcast=current.rnx.gz \
    --output ./staging

``fetch --url ROLE=URL`` sets a source URL fallback chain. Repeat the option
for each fallback URL. ``process --manifest PATH`` selects a manifest.
``--report PATH`` selects a separate report path. ``--fit-rms METRES`` sets
the orbit fit limit; the default is 1 metre. The quantised orbit check permits
an additional 0.5 metre.

Use ``--at 2026-09-23T12:00:00Z`` for a reproducible build time. The source
data must cover that time and the requested sample windows. This option also
sets the ZIP timestamp; it does not make old data current.

The processor checks the data format, satellite counts, time coverage and
source policy before packing. ANTEX corrections use the satellite's radial
phase-centre offset; transverse antenna offsets are not modelled.

If processing refuses output, it saves the report and removes
``ephemeris.zip`` in the selected staging directory. Read entries with
``status: fail`` in the report. Use a private staging directory for processing.
The NixOS module below manages a separate published directory.

Exit status is 0 for success, 2 for a command-line parsing error, 3 when a
source is unavailable, 4 when output is refused, and 1 for another error.

NixOS service
-------------

Import ``nixosModules.default`` from this flake. The alias
``nixosModules.weinav-forge`` provides the same module. For example, with the
flake available as the ``weinav-forge`` input::

  {
    imports = [ inputs.weinav-forge.nixosModules.default ];

    services.weinav-forge = {
      enable = true;
      instances.watch = {
        flavor = "huawei-plus";
      };
      nginx = {
        enable = true;
        virtualHost = "gnss.example.org";
        location = "/agnss/";
      };
    };

    services.nginx.virtualHosts."gnss.example.org" = {
      enableACME = true;
      forceSSL = true;
    };
    security.acme.acceptTerms = true;
    security.acme.defaults.email = "admin@example.org";
    networking.firewall.allowedTCPPorts = [ 80 443 ];
  }

This instance serves ``/agnss/watch/ephemeris.zip``. Nginx selects the
identity, gzip or Brotli file from the same immutable generation. Each
response has a no-store cache policy. Source files, reports, and generation
directories have no public URL.

The fetch timer runs every ten minutes, with up to 30 seconds of jitter.
The fetch service obtains the union of source roles required by the enabled
instances. After a successful fetch, systemd starts the build service.
The build service has no network access and reads the cache through a
read-only mount. Both services run as separate unprivileged users with
restricted system calls and write access.

Each build first writes private staging files. It then makes gzip and Brotli
sidecars with ``pigz`` and ``brotli``, and checks both by decompression.
The completed set moves to a generation directory on the output filesystem.
A single atomic link replacement publishes the set. The ZIP keeps its
original timestamp. Failed updates and reboots keep the previous published
generation, including output whose timestamp has expired.

Common options under ``services.weinav-forge`` are:

* ``package``: executable package override.
* ``cacheDirectory``: default ``/var/cache/weinav-forge``.
* ``stateDirectory``: default ``/var/lib/weinav-forge``.
* ``outputDirectory``: default ``/var/lib/weinav-forge-public``.
* ``instances.NAME``: ``enable``, ``flavor``, ``systems``, ``agnss``,
  ``fitRms`` and ``allowDegraded``.
* ``fetch.timerConfig``: systemd timer settings. Set ``OnCalendar`` to change
  the schedule. ``fetch.sourceUrls`` sets URL fallback chains by source role.
* ``fetch.timeout`` and ``build.timeout``: default 15 minutes each.
* ``compression.threads``: pigz thread count, default 1. Brotli uses one
  thread. ``compression.gzip.level`` and ``compression.brotli.quality``
  both default to 6. Each format has an ``enable`` and a ``package`` option.
* ``limits.memoryMax``, ``limits.cpuQuota`` and ``limits.tasksMax``: default
  ``1G``, ``100%`` and 64 for each service.
* ``retention.generations``: keep at least 3 recent generations per instance.
  ``retention.minimumGenerationAgeHours`` defaults to 24. Cleanup always
  keeps the current generation, regardless of its age.
* ``retention.cacheMaxAgeHours``: default 168. Cleanup removes old URL
  metadata and old objects that no committed manifest or current URL
  metadata references.
* ``nginx.enable``, ``nginx.virtualHost`` and ``nginx.location``: configure
  the archive locations. Configure TLS, listeners and the firewall through
  the standard NixOS options.

The three data directories must be separate. Do not use one as a parent of
another. The module owns these directories and their cleanup. A shared lock
prevents fetching, building and cleanup at the same time. Increase the timer
interval or service limits for many instances or a slower machine.

Start a build from the committed cache with::

  systemctl start weinav-forge-build.service

Read service results and reports with::

  journalctl -u weinav-forge-fetch -u weinav-forge-build
  cat /var/lib/weinav-forge/reports/watch.json
