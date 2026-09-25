{ pkgs, module }:
let
  cache = "/var/cache/weinav-forge";
  state = "/var/lib/weinav-forge";
  public = "/var/lib/weinav-forge-public";
  fixture = pkgs.writeShellApplication {
    name = "weinav-forge";
    runtimeInputs = with pkgs; [
      coreutils
      curl
      jq
      zip
    ];
    text = ''
      command=$1
      shift
      output=""
      report=""
      flavor=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          --output) output=$2; shift 2 ;;
          --report) report=$2; shift 2 ;;
          --flavor) flavor=$2; shift 2 ;;
          --no-agnss|--allow-degraded) shift ;;
          *) shift 2 ;;
        esac
      done
      if [ "$command" = fetch ]; then
        mkdir -p ${cache}/objects ${cache}/urls
        curl -fsS --max-time 10 http://127.0.0.1:8081/source > ${cache}/source.new
        chmod 0640 ${cache}/source.new
        mv -f ${cache}/source.new ${cache}/source
        id -u > ${cache}/fetch-uid
        if chmod 0777 ${cache} 2>/dev/null; then exit 95; fi
        if rm /run/weinav-forge/lock 2>/dev/null; then exit 96; fi
        if touch ${public}/fetch-must-not-write 2>/dev/null; then exit 90; fi
        if touch /etc/fetch-must-not-write 2>/dev/null; then exit 91; fi
        exit 0
      fi
      if curl -fsS --max-time 1 http://127.0.0.1:8081/source >/dev/null 2>&1; then exit 92; fi
      if touch ${cache}/build-must-not-write 2>/dev/null; then exit 93; fi
      if touch /etc/build-must-not-write 2>/dev/null; then exit 94; fi
      id -u > ${state}/build-uid
      if chmod 0777 ${state} 2>/dev/null; then exit 97; fi
      if chmod 0777 ${public} 2>/dev/null; then exit 98; fi
      if [ -f ${cache}/pause-process ]; then
        touch ${state}/paused
        sleep 120
      fi
      if [ -f ${cache}/fail-process ] || { [ "$flavor" = open-plus ] && [ -f ${cache}/fail-secondary ]; }; then
        printf '{"status":"refused"}\n' > "$report"
        exit 4
      fi
      mkdir -p "$output"
      stamp=$(date +%s%3N)
      if [ -f ${cache}/old-time ]; then stamp=1600000000000; fi
      printf '%s' "$stamp" > "$output/time"
      cat ${cache}/source > "$output/payload"
      jq -n --argjson stamp "$stamp" '{status:"passed",timestamp_ms:$stamp}' > "$report"
      cd "$output"
      zip -q -0 ephemeris.zip time payload
    '';
  };
  compressor =
    name: package:
    pkgs.writeShellApplication {
      inherit name;
      runtimeInputs = [ pkgs.coreutils ];
      text = ''
        source=''${!#}
        if [ -f ${cache}/fail-compression ]; then exit 1; fi
        if [ -f ${cache}/fail-manifest-compression ] && [[ "$source" = */manifest.json ]]; then exit 1; fi
        if [ -f ${cache}/compression-size ]; then
          if [ "$1" = -d ]; then
            cat "''${source%.*}"
          else
            cat "$source"
            if [ "$(cat ${cache}/compression-size)" = larger ]; then
              printf padding
            fi
          fi
          exit 0
        fi
        exec ${package}/bin/${name} "$@"
      '';
    };
  reader = pkgs.writeText "weinav-reader.py" ''
    import gzip
    import hashlib
    import io
    import json
    import subprocess
    import time
    import urllib.request
    import zipfile

    for index in range(150):
        encoding = ["identity", "gzip", "br"][index % 3]
        request = urllib.request.Request("http://localhost/agnss/watch/ephemeris.zip", headers={"Accept-Encoding": encoding})
        with urllib.request.urlopen(request) as response:
            data = response.read()
            assert response.headers.get("Content-Encoding", "identity") == encoding
        if encoding == "gzip":
            data = gzip.decompress(data)
        elif encoding == "br":
            data = subprocess.run(["${pkgs.brotli}/bin/brotli", "-dc"], input=data, capture_output=True, check=True).stdout
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            assert archive.testzip() is None
            assert archive.read("payload") in [b"second-generation", b"third-generation"]
            assert len(archive.read("time")) == 13
        with urllib.request.urlopen("http://localhost/agnss/manifest.json") as response:
            manifest = json.load(response)
        for variant in manifest["variants"]:
            with urllib.request.urlopen("http://localhost" + variant["url"]) as response:
                archive = response.read()
            assert len(archive) == variant["size"]
            assert hashlib.sha256(archive).hexdigest() == variant["sha256"]
        time.sleep(0.02)
    open("/tmp/reader-done", "w").close()
  '';
in
pkgs.testers.runNixOSTest {
  name = "weinav-forge-publication";
  requiredFeatures.kvm = false;
  nodes.machine = { ... }: {
    imports = [ module ];
    virtualisation.memorySize = 1536;
    services.weinav-forge = {
      enable = true;
      package = fixture;
      instances.watch = {
        flavor = "huawei";
        systems = [ "gps" ];
      };
      instances.secondary = {
        flavor = "open-plus";
        systems = [
          "gps"
          "galileo"
        ];
        agnss = false;
      };
      compression.gzip.package = compressor "pigz" pkgs.pigz;
      compression.brotli.package = compressor "brotli" pkgs.brotli;
      fetch.timerConfig = {
        OnBootSec = "3s";
        OnUnitActiveSec = "10min";
      };
      nginx.enable = true;
      nginx.virtualHost = "localhost";
    };
    systemd.services.fixture-source = {
      wantedBy = [ "multi-user.target" ];
      before = [ "weinav-forge-fetch.service" ];
      serviceConfig = {
        ExecStart = "${pkgs.python3}/bin/python -m http.server 8081 --bind 127.0.0.1 --directory /srv/fixture";
        DynamicUser = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_UNIX"
        ];
      };
    };
    systemd.tmpfiles.rules = [
      "d /srv/fixture 0755 root root -"
      "f /srv/fixture/source 0644 root root - first-generation"
    ];
    environment.systemPackages = with pkgs; [
      acl
      curl
      pigz
      brotli
      unzip
      jq
    ];
  };
  testScript = ''
    import json

    machine.start(allow_reboot=True)
    machine.wait_for_unit("nginx.service")
    machine.wait_for_unit("fixture-source.service")
    try:
        machine.wait_until_succeeds("test -L ${public}/watch/current", timeout=120)
    except Exception:
        print(machine.succeed("journalctl -b -u weinav-forge-fetch -u weinav-forge-build --no-pager"))
        raise
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = inactive")
    machine.succeed("systemctl stop weinav-forge-fetch.timer")

    def current():
        return machine.succeed("readlink ${public}/watch/current").strip()

    def digest():
        return machine.succeed("sha256sum ${public}/watch/current/ephemeris.zip").split()[0]

    def check_manifest(expected=("secondary", "watch")):
        manifest = json.loads(machine.succeed("curl -fsS http://localhost/agnss/manifest.json"))
        assert manifest["version"] == 1
        assert [variant["name"] for variant in manifest["variants"]] == list(expected)
        for variant in manifest["variants"]:
            machine.succeed(f"curl -fsS http://localhost{variant['url']} -o /tmp/variant.zip")
            assert machine.succeed("sha256sum /tmp/variant.zip").split()[0] == variant["sha256"]
            assert int(machine.succeed("stat -c %s /tmp/variant.zip")) == variant["size"]
            assert int(machine.succeed("unzip -p /tmp/variant.zip time")) == variant["timestamp_ms"]
            assert variant["latest_url"] == f"/agnss/{variant['name']}/ephemeris.zip"
            assert variant["generation"] in variant["url"]
            if variant["name"] == "watch":
                assert variant["flavor"] == "huawei" and variant["systems"] == ["gps"] and variant["agnss"]
            else:
                assert variant["flavor"] == "open-plus" and variant["systems"] == ["gps", "galileo"] and not variant["agnss"]
        return manifest

    def check_encodings():
        for url, path, mime in [
            ("/agnss/watch/ephemeris.zip", "${public}/watch/current/ephemeris.zip", "application/zip"),
            ("/agnss/manifest.json", "${public}/manifest.json", "application/json"),
        ]:
            for encoding, suffix, command in [("identity", "", "cat"), ("gzip", ".gz", "pigz -dc"), ("br", ".br", "brotli -dc")]:
                exists = encoding != "identity" and machine.execute(f"test -f {path}{suffix}")[0] == 0
                machine.succeed(f"curl -fsS -D /tmp/headers -H 'Accept-Encoding: {encoding}' http://localhost{url} -o /tmp/encoded")
                headers = machine.succeed("cat /tmp/headers").lower()
                assert f"content-type: {mime}" in headers
                assert "cache-control: no-store" in headers
                assert "vary: accept-encoding" in headers
                if exists:
                    assert f"content-encoding: {encoding}" in headers
                    assert int(machine.succeed(f"stat -Lc %s {path}{suffix}")) < int(machine.succeed(f"stat -Lc %s {path}"))
                else:
                    assert "content-encoding:" not in headers
                    command = "cat"
                machine.succeed(f"{command} /tmp/encoded > /tmp/decoded; cmp /tmp/decoded {path}")
        check_manifest()

    check_encodings()
    first_manifest = check_manifest()
    machine.succeed("test -f ${public}/manifest.json.gz; test -f ${public}/manifest.json.br")
    machine.fail("runuser -u nginx -- sh -c 'echo broken > ${public}/manifest.json'")
    for path in ["/agnss/.manifest/current/manifest.json", "/agnss/watch/current/variant.json", "/agnss/manifest.json.gz"]:
        assert machine.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' http://localhost{path}").strip() == "404"
    first = current()
    first_digest = digest()
    machine.fail("runuser -u nginx -- cat ${cache}/source")
    machine.fail("runuser -u nginx -- cat ${state}/reports/watch.json")
    machine.fail("runuser -u nginx -- touch ${public}/nginx-must-not-write")
    machine.fail("runuser -u nginx -- touch ${public}/watch/current/nginx-must-not-write")
    machine.fail("runuser -u nginx -- sh -c 'echo broken > ${public}/watch/current/ephemeris.zip'")
    for service in ["fetch", "build"]:
        assert machine.succeed(f"systemctl show -p DynamicUser --value weinav-forge-{service}").strip() == "yes"
        machine.fail(f"grep '^weinav-{service}:' /etc/passwd")
        machine.fail(f"getent passwd weinav-{service}")
    machine.succeed("touch ${cache}/pause-process; systemctl start --no-block weinav-forge-build")
    machine.wait_until_succeeds("test -e ${state}/paused")
    machine.succeed("systemctl kill --signal=KILL weinav-forge-build")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = failed")
    machine.succeed("rm ${cache}/pause-process ${state}/paused; systemctl reset-failed weinav-forge-build")
    fetch_uid = int(machine.succeed("cat ${cache}/fetch-uid"))
    build_uid = int(machine.succeed("cat ${state}/build-uid"))
    assert fetch_uid != build_uid
    for role, uid in [("fetch", fetch_uid), ("build", build_uid)]:
        assert 61184 <= uid <= 65519
        machine.succeed(f"useradd --uid {uid} --no-create-home --no-user-group recycled-{role}")
        for path in ["${cache}/source", "${state}/reports/watch.json", "${public}/watch/current/ephemeris.zip", "/run/weinav-forge/lock"]:
            machine.fail(f"runuser -u recycled-{role} -- cat {path}")
    machine.succeed("test $(curl -s -o /dev/null -w '%{http_code}' http://localhost/agnss/watch/current/ephemeris.zip) = 404")
    machine.succeed("test $(curl -s -o /dev/null -w '%{http_code}' http://localhost/agnss/watch/.generations/) = 404")
    machine.succeed("systemd-analyze security --no-pager weinav-forge-fetch.service weinav-forge-build.service > /tmp/security.txt")
    print(machine.succeed("cat /tmp/security.txt"))
    assert machine.succeed("systemctl show -p PrivateNetwork --value weinav-forge-build").strip() == "yes"
    assert "~@network-io" in machine.succeed("systemctl cat weinav-forge-build")

    machine.succeed("touch ${cache}/fail-process")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    assert check_manifest() == first_manifest
    machine.succeed("rm ${cache}/fail-process; touch ${cache}/fail-compression")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    assert check_manifest() == first_manifest
    machine.succeed("rm ${cache}/fail-compression; touch ${cache}/old-time")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    assert check_manifest() == first_manifest
    machine.succeed("rm ${cache}/old-time")
    machine.succeed("mkdir ${public}/watch/.generations/.stage.interrupted")
    machine.succeed("printf second-generation > /srv/fixture/source")
    machine.succeed("systemctl reset-failed weinav-forge-build; systemctl start weinav-forge-fetch")
    machine.wait_until_succeeds(f"test $(readlink ${public}/watch/current) != {first}")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = inactive")
    assert digest() != first_digest
    machine.succeed("test ! -e ${public}/watch/.generations/.stage.interrupted")
    check_encodings()

    assert int(machine.succeed("cat ${cache}/fetch-uid")) != fetch_uid
    assert int(machine.succeed("cat ${state}/build-uid")) != build_uid
    machine.succeed("test -z \"$(ls -A ${state}/staging)\"")

    machine.succeed("systemd-run --unit=weinav-reader ${pkgs.python3}/bin/python ${reader}")
    machine.succeed("systemd-run --unit=hold-lock -p RuntimeMaxSec=30s ${pkgs.util-linux}/bin/flock /run/weinav-forge/lock ${pkgs.bash}/bin/bash -c '${pkgs.coreutils}/bin/touch /tmp/lock-held; while [ ! -e /tmp/release-lock ]; do ${pkgs.coreutils}/bin/sleep 0.1; done'")
    machine.wait_until_succeeds("test -e /tmp/lock-held")
    before = current()
    machine.succeed("printf third-generation > /srv/fixture/source")
    machine.succeed("systemctl start --no-block weinav-forge-build weinav-forge-fetch")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = activating")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-fetch) = activating")
    assert current() == before
    machine.succeed("touch /tmp/release-lock")
    machine.wait_until_succeeds("test $(unzip -p ${public}/watch/current/ephemeris.zip payload) = third-generation")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = inactive")
    machine.wait_until_succeeds("test -e /tmp/reader-done", timeout=30)
    check_encodings()

    before = current()
    before_digest = digest()
    machine.succeed("useradd --system --gid weinav-forge-cache weinav-forge-fetch")
    machine.succeed("useradd --system --gid weinav-forge-cache weinav-forge-build")
    for path, owner, group, mode in [
        ("${cache}", "weinav-forge-fetch", "weinav-forge-cache", "2750"),
        ("${state}", "weinav-forge-build", "weinav-forge-cache", "0700"),
        ("${public}", "weinav-forge-build", "weinav-forge-public", "2750"),
    ]:
        machine.succeed(f"setfacl -Rb {path}; chown -R {owner}:{group} {path}; find {path} -type d -exec chmod {mode} {{}} +")
    machine.succeed("systemd-tmpfiles --create --prefix=${cache} --prefix=${state} --prefix=${public}")
    for path in ["${cache}", "${state}", "${public}"]:
        assert machine.succeed(f"stat -c %u {path}").strip() == "0"
    assert current() == before and digest() == before_digest
    check_encodings()
    machine.succeed("systemctl start weinav-forge-fetch")
    machine.wait_until_succeeds(f"test $(readlink ${public}/watch/current) != {before}")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = inactive")
    assert int(machine.succeed("cat ${cache}/fetch-uid")) >= 61184
    assert int(machine.succeed("cat ${state}/build-uid")) >= 61184
    check_encodings()

    for size in ["equal", "larger"]:
        machine.succeed(f"printf {size} > ${cache}/compression-size; systemctl start weinav-forge-build")
        for path in ["${public}/watch/current/ephemeris.zip", "${public}/secondary/current/ephemeris.zip", "${public}/manifest.json"]:
            machine.fail(f"test -e {path}.gz")
            machine.fail(f"test -e {path}.br")
        check_encodings()
    machine.succeed("rm ${cache}/compression-size; systemctl start weinav-forge-build")
    check_encodings()

    machine.succeed("cp ${public}/watch/current/ephemeris.zip ${public}/watch/current/ephemeris.zip.gz; cp ${public}/watch/current/ephemeris.zip ${public}/watch/current/ephemeris.zip.br; touch ${cache}/fail-process")
    machine.fail("systemctl start weinav-forge-build")
    machine.fail("test -e ${public}/watch/current/ephemeris.zip.gz")
    machine.fail("test -e ${public}/watch/current/ephemeris.zip.br")
    check_encodings()
    machine.succeed("rm ${cache}/fail-process; systemctl reset-failed weinav-forge-build")

    previous_manifest = check_manifest()
    secondary = previous_manifest["variants"][0]
    machine.succeed("touch ${cache}/fail-secondary")
    machine.fail("systemctl start weinav-forge-build")
    partial_manifest = check_manifest()
    assert partial_manifest["variants"][0] == secondary
    assert partial_manifest["variants"][1]["generation"] != previous_manifest["variants"][1]["generation"]
    machine.succeed("rm ${public}/secondary/current")
    machine.fail("systemctl start weinav-forge-build")
    check_manifest(expected=("watch",))
    machine.succeed("rm ${cache}/fail-secondary; systemctl reset-failed weinav-forge-build; systemctl start weinav-forge-build")
    check_encodings()

    previous_manifest = check_manifest()
    machine.succeed("touch ${cache}/fail-manifest-compression")
    for variant in previous_manifest["variants"]:
        machine.succeed(f"touch -d '3 days ago' ${public}/{variant['name']}/.generations/{variant['generation']}")
    for attempt in range(4):
        machine.succeed("systemctl reset-failed weinav-forge-build")
        machine.fail("systemctl start weinav-forge-build")
        assert check_manifest() == previous_manifest
    assert current().split("/")[-1] != previous_manifest["variants"][1]["generation"]
    machine.succeed("rm ${cache}/fail-manifest-compression; systemctl reset-failed weinav-forge-build; systemctl start weinav-forge-build")
    assert check_manifest() != previous_manifest
    check_encodings()

    machine.succeed("systemctl stop fixture-source")
    before = current()
    machine.fail("systemctl start weinav-forge-fetch")
    assert current() == before
    machine.succeed("systemctl start weinav-forge-build")
    check_encodings()

    machine.succeed("touch ${cache}/fail-process")
    machine.succeed("date -s '+2 hours'")
    expired_digest = digest()
    expired_time = machine.succeed("unzip -p ${public}/watch/current/ephemeris.zip time")
    machine.fail("systemctl start weinav-forge-build")
    assert digest() == expired_digest
    check_encodings()
    machine.reboot()
    machine.wait_for_unit("nginx.service")
    assert digest() == expired_digest
    assert machine.succeed("unzip -p ${public}/watch/current/ephemeris.zip time") == expired_time
    check_encodings()
  '';
}
