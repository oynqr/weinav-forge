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
      while [ "$#" -gt 0 ]; do
        case "$1" in
          --output) output=$2; shift 2 ;;
          --report) report=$2; shift 2 ;;
          --no-agnss|--allow-degraded) shift ;;
          *) shift 2 ;;
        esac
      done
      if [ "$command" = fetch ]; then
        mkdir -p ${cache}/objects ${cache}/urls
        curl -fsS --max-time 10 http://127.0.0.1:8081/source > ${cache}/source
        chmod 0640 ${cache}/source
        if touch ${public}/fetch-must-not-write 2>/dev/null; then exit 90; fi
        if touch /etc/fetch-must-not-write 2>/dev/null; then exit 91; fi
        exit 0
      fi
      if curl -fsS --max-time 1 http://127.0.0.1:8081/source >/dev/null 2>&1; then exit 92; fi
      if touch ${cache}/build-must-not-write 2>/dev/null; then exit 93; fi
      if touch /etc/build-must-not-write 2>/dev/null; then exit 94; fi
      if [ -f ${cache}/fail-process ]; then
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
  pigz = pkgs.writeShellApplication {
    name = "pigz";
    text = ''
      if [ -f ${cache}/fail-compression ]; then exit 1; fi
      exec ${pkgs.pigz}/bin/pigz "$@"
    '';
  };
  reader = pkgs.writeText "weinav-reader.py" ''
    import gzip
    import io
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
      compression.gzip.package = pigz;
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
      curl
      pigz
      brotli
      unzip
      jq
    ];
  };
  testScript = ''
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

    def check_encodings():
        for encoding, command in [("identity", "cat"), ("gzip", "pigz -dc"), ("br", "brotli -dc")]:
            machine.succeed(f"curl -fsS -D /tmp/headers -H 'Accept-Encoding: {encoding}' http://localhost/agnss/watch/ephemeris.zip -o /tmp/encoded")
            headers = machine.succeed("cat /tmp/headers").lower()
            assert "content-type: application/zip" in headers
            assert "cache-control: no-store" in headers
            assert "vary: accept-encoding" in headers
            if encoding == "identity":
                assert "content-encoding:" not in headers
            else:
                assert f"content-encoding: {encoding}" in headers
            machine.succeed(f"{command} /tmp/encoded > /tmp/decoded; cmp /tmp/decoded ${public}/watch/current/ephemeris.zip")

    check_encodings()
    first = current()
    first_digest = digest()
    machine.fail("runuser -u nginx -- cat ${cache}/source")
    machine.fail("runuser -u nginx -- cat ${state}/reports/watch.json")
    machine.succeed("test $(curl -s -o /dev/null -w '%{http_code}' http://localhost/agnss/watch/current/ephemeris.zip) = 404")
    machine.succeed("test $(curl -s -o /dev/null -w '%{http_code}' http://localhost/agnss/watch/.generations/) = 404")
    machine.succeed("systemd-analyze security --no-pager weinav-forge-fetch.service weinav-forge-build.service > /tmp/security.txt")
    print(machine.succeed("cat /tmp/security.txt"))
    assert machine.succeed("systemctl show -p PrivateNetwork --value weinav-forge-build").strip() == "yes"
    assert "~@network-io" in machine.succeed("systemctl cat weinav-forge-build")

    machine.succeed("touch ${cache}/fail-process")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    machine.succeed("rm ${cache}/fail-process; touch ${cache}/fail-compression")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    machine.succeed("rm ${cache}/fail-compression; touch ${cache}/old-time")
    machine.fail("systemctl start weinav-forge-build")
    assert current() == first and digest() == first_digest
    machine.succeed("rm ${cache}/old-time")
    machine.succeed("mkdir ${public}/watch/.generations/.stage.interrupted")
    machine.succeed("printf second-generation > /srv/fixture/source")
    machine.succeed("systemctl reset-failed weinav-forge-build; systemctl start weinav-forge-fetch")
    machine.wait_until_succeeds(f"test $(readlink ${public}/watch/current) != {first}")
    machine.wait_until_succeeds("test $(systemctl show -p ActiveState --value weinav-forge-build) = inactive")
    assert digest() != first_digest
    machine.succeed("test ! -e ${public}/watch/.generations/.stage.interrupted")
    check_encodings()

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
