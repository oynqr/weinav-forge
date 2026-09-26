weinav-forge
============

Build GNSS assistance data for Huawei watches and Gadgetbridge.
One Rust executable has two commands:

* ``fetch`` gets source files and saves them in a cache.
* ``process`` reads local files, builds the products, checks the results, and
  writes ``ephemeris.zip``, ``report.json`` and the broadcast health file. It
  does not use the network.

Build
-----

Use Nix to build the Linux executable:

.. code-block:: bash

  nix build
  ./result/bin/weinav-forge --help

The flake supports ``x86_64-linux`` and ``aarch64-linux``.

Choose a flavor
---------------

Assistance data helps a watch find satellites and calculate its position.
A flavor selects the sources for this data. Public sources provide data
without using Huawei's download services.

``huawei``
  Build the assistance files from Huawei data. Use public data only to
  check that the satellites are usable.

``huawei-plus``
  Combine Huawei data with satellite position predictions from public
  sources. Keep Huawei's data for finding satellites when the watch starts.

``open-plus``
  Use public data for finding satellites and predicting their positions.
  Use Huawei data for some additional information and to fill gaps in the
  predictions. This flavor still needs Huawei data.

``open``
  Use public sources only, with no Huawei data. The files lack approximate
  position lists for BeiDou and GLONASS, plus some time and satellite status
  data. BeiDou and QZSS predictions can cover less than three days.

  The watch can take longer to find its first position, especially with
  weak signals, because it may need more data directly from satellites.
  These gaps do not disable the affected satellite systems.
  Use ``process --allow-degraded`` to permit these gaps.

Each flavor must pass checks before the tool writes a ZIP file. A build can
fail if its sources have too few usable satellites. Read ``report.json`` for
the results.

Use ``--plan`` to see the detailed source policy without reading or
downloading source files:

.. code-block:: bash

  weinav-forge fetch --flavor huawei-plus --plan

Fetch and process
-----------------

For example:

.. code-block:: bash

  weinav-forge fetch --flavor huawei-plus --cache ./cache
  weinav-forge process --flavor huawei-plus --cache ./cache --output ./staging

Use the same cache directory for both commands. Run ``fetch`` again to get
new source data before the next build.

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

For example:

.. code-block:: bash

  weinav-forge process --flavor huawei --systems gps --no-agnss \
    --source seed=HiEE_V2.dat \
    --source broadcast=previous.rnx.gz --source broadcast=current.rnx.gz \
    --output ./staging

``fetch --url ROLE=URL`` sets a source URL fallback chain. Repeat the option
for each fallback URL. ``process --manifest PATH`` selects a manifest.
``--report PATH`` selects a separate report path. ``--fit-rms METRES`` sets
the orbit fit limit; the default is 1 metre.

Use ``--at 2026-09-23T12:00:00Z`` for a reproducible build time. The source
data must cover that time and the requested time range. This option also
sets the ZIP timestamp; it does not make old data current.

If processing refuses output, it saves the report and removes
``ephemeris.zip`` in the selected staging directory. Read entries with
``status: fail`` in the report. Use a private staging directory for processing.
The NixOS module below manages a separate published directory.

The broadcast health file is a copy of the broadcast data that the tool used
to find unhealthy satellites.

Exit status is 0 for success, 2 for a command-line parsing error, 3 when a
source is unavailable, 4 when output is refused, and 1 for another error.

Build multiple variants
-----------------------

Use ``process --variants PATH`` to build a list of variants in one command.
For example, save this JSON list as ``variants.json``:

.. code-block:: json

  [
    {
      "name": "watch",
      "flavor": "huawei-plus"
    },
    {
      "name": "gps-only",
      "flavor": "huawei",
      "systems": ["gps"],
      "agnss": false
    }
  ]

Fetch the sources for each flavor, then run:

.. code-block:: bash

  weinav-forge process --variants variants.json --cache ./cache \
    --output ./staging --threads 2

Each variant gets ``ephemeris.zip`` and ``report.json`` in
``./staging/NAME/``. Names must be unique. Use only ASCII letters, digits,
underscores and hyphens. The first character must be a letter or digit.

Each entry requires ``name`` and ``flavor``. Optional fields are ``systems``
(all five by default), ``agnss`` (true), ``fit_rms`` (1 metre), and
``allow_degraded`` (false). Use the constellation names shown above.
Set these fields in the file. Do not combine ``--variants`` with individual
variant options, ``--source``, ``--manifest`` or ``--report``.
``--at`` applies to all variants. ``--plan`` prints their plans in list order.

``--threads`` applies to single builds and to variant lists. It sets the
maximum number of processing threads. The default is the available CPU
thread count. Variants share this limit. More threads can reduce build time
and increase memory use. Use ``--threads 1`` to process on one thread.

A failed variant does not stop the other variants. Exit status is 1 if any
variant has an error, otherwise 4 if any output is refused, otherwise 3 if
any source is unavailable, otherwise 0.

NixOS service
-------------

Import ``nixosModules.default`` from this flake. The alias
``nixosModules.weinav-forge`` provides the same module. For example, with the
flake available as the ``weinav-forge`` input:

.. code-block:: nix

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

    networking.firewall.allowedTCPPorts = [
      80
      443
    ];
  }

This instance serves ``/agnss/watch/ephemeris.zip``. Add an instance for each
required flavor or constellation selection. Each instance has its own URL:
``/agnss/NAME/ephemeris.zip``, where ``NAME`` is the instance name. By default,
each instance includes all five constellations and AGNSS.

The service fetches data every ten minutes, with a random delay of up to
30 seconds. After a successful fetch, it builds the configured instances.
Each instance publishes a new ZIP only when its checks pass. Failed updates
and reboots keep its previous ZIP available, even after expiry. The ZIP keeps
its original timestamp.

Read ``/agnss/manifest.json`` to find the available variants. If you change
``nginx.location``, use that prefix instead of ``/agnss/``. The JSON object
has ``version`` set to 1 and a ``variants`` list. Each entry has these fields:

* ``name``: instance name.
* ``url``: URL for this file version, on the same server.
* ``latest_url``: URL that always selects the latest published file.
* ``generation``: file version identifier.
* ``sha256``: SHA-256 checksum of the ZIP file, in hexadecimal.
* ``size``: ZIP file size in bytes.
* ``timestamp_ms``: original ZIP timestamp, in milliseconds since the Unix
  epoch.
* ``flavor``, ``systems`` and ``agnss``: source policy, constellations and
  AGNSS selection. Older files can lack these three fields until the next
  successful build.
* ``report_url``: URL of the build report for this file version.
* ``broadcast_url``: URL of the broadcast health file for this file version.
  Older files can lack ``report_url`` and ``broadcast_url``.

Use ``url`` to download a file and check its checksum. The file at
``latest_url`` can change after you read the manifest. Read the manifest
again if a file version is no longer available. The current manifest keeps
its listed file versions available.

An instance appears only after it has a published ZIP. A failed update keeps
the previous entry, including an expired file and its original timestamp.
If the manifest update fails, the previous manifest stays available.

Clients can cache each file for five minutes. The latest URLs support ETags
to check for changes. URLs with generation identifiers are marked immutable.

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
* ``build.threads``: processing thread limit shared by all instances,
  default ``null``. This selects the available CPU thread count when the
  service starts. All instances build in one command.
* ``limits.memoryMax``: default ``1G`` for each service.
* ``limits.cpuQuota``: default ``""`` (no quota) with automatic thread
  selection, or ``100%`` per thread with an explicit ``build.threads``.
* ``limits.tasksMax``: default ``"infinity"`` with automatic thread
  selection. An explicit ``build.threads`` sets the default to at least 64
  tasks and 16 more than the thread count. This limit applies to each service.
* ``retention.generations``: keep at least 3 recent output versions per
  instance. ``retention.minimumGenerationAgeHours`` defaults to 24. Cleanup
  always keeps the published version, regardless of its age.
* ``retention.cacheMaxAgeHours``: age limit for unused cached files, default
  168 hours.
* ``nginx.enable``, ``nginx.virtualHost`` and ``nginx.location``: configure
  the archive locations. Configure TLS, listeners and the firewall through
  the standard NixOS options.

The three data directories must be separate. Do not use one as a parent of
another. Use a filesystem with POSIX access control lists for these
directories. The module manages these directories and their cleanup.
Increase the timer interval or service limits for many instances or a slower
machine.

Start a build from the cached source files with:

.. code-block:: bash

  systemctl start weinav-forge-build.service

Read service results and reports with:

.. code-block:: bash

  journalctl -u weinav-forge-fetch -u weinav-forge-build
  cat /var/lib/weinav-forge/reports/watch.json

The service keeps each variant's latest report in the ``reports`` directory,
including reports for refused output. Each published file version also
includes its report and its broadcast health file. The journal shows the
saved report path and each failed check. Temporary report paths from
processing are removed at the end of the build.
