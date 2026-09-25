weinav-forge
============

Build GNSS assistance data for Huawei watches and Gadgetbridge.
One Rust executable has two commands:

* ``fetch`` gets source files and saves them in a cache.
* ``process`` reads local files, builds the products, checks the results, and
  writes ``ephemeris.zip`` and ``report.json``. It does not use the network.

Build
-----

Use Nix to build the Linux executable::

  nix build
  ./result/bin/weinav-forge --help

The flake supports ``x86_64-linux`` and ``aarch64-linux``.

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

The report lists source files, data coverage and check results. A flavor can
fail its checks when current sources have too few usable satellites.

Fetch and process
-----------------

For example::

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

For example::

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

This instance serves ``/agnss/watch/ephemeris.zip``. Add an instance for each
required flavor or constellation selection. Each instance has its own URL:
``/agnss/NAME/ephemeris.zip``, where ``NAME`` is the instance name. By default,
each instance includes all five constellations and AGNSS.

The service fetches data every ten minutes, with a random delay of up to
30 seconds. After a successful fetch, it builds the configured instances.
Each instance publishes a new ZIP only when its checks pass. Failed updates
and reboots keep its previous ZIP available, even after expiry. The ZIP keeps
its original timestamp.

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
* ``limits.memoryMax``, ``limits.cpuQuota`` and ``limits.tasksMax``: default
  ``1G``, ``100%`` and 64 for each service.
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

Start a build from the cached source files with::

  systemctl start weinav-forge-build.service

Read service results and reports with::

  journalctl -u weinav-forge-fetch -u weinav-forge-build
  cat /var/lib/weinav-forge/reports/watch.json
