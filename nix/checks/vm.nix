# NixOS VM tests of the real service. Each runs the step 11 module on a machine with user
# `alice`; the indexer is started by the test script after it has laid out its files.
{
  pkgs,
  lib,
  package,
  nixosModule,
  src,
}:
let
  tests = "${src}/tests";
  exe = "${package}/bin/llm-index";

  localRoot = path: tier: {
    inherit path tier;
    kind = "local";
    interval = "30s";
  };

  node =
    {
      instance,
      extra ? { },
    }:
    {
      imports = [
        nixosModule
        extra
      ];
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
      users.users.bob.isNormalUser = true;
      services.llm-index.instances.alice = {
        inherit package;
        limits.workers = 2;
      }
      // instance;
      systemd.services.llm-index-alice.wantedBy = lib.mkForce [ ];
      environment.systemPackages = [
        package
        pkgs.python3
      ];
      virtualisation.memorySize = 2048;
    };

  # Python helpers shared by every test script.
  preamble = ''
    import base64
    import datetime as dt
    import json
    import shlex

    EXE = "${exe}"
    CFG = "/etc/llm-index/alice.toml"
    TESTS = "${tests}"
    GENERIC = "index configuration is being checked"

    def as_alice(m, cmd, **kw):
        return m.succeed(f"sudo -u alice sh -c {shlex.quote(cmd)}", **kw)

    def mcp(m, tier, tool, args=None):
        """(is_error, text) of one tool call."""
        status, out = m.execute(
            f"sudo -u alice python3 {TESTS}/mcp_client.py {EXE} {CFG} {tier} {tool} {json.dumps(json.dumps(args or {}))}"
        )
        assert status in (0, 1), out
        return status == 1, out.strip()

    def status(m):
        return m.succeed(f"sudo -u alice {EXE} status --config {CFG}")

    def wait_indexed(m, root):
        m.wait_until_succeeds(
            f"sudo -u alice {EXE} status --config {CFG} | grep -E '^{root} .* ok .*queued/done: 0/[1-9]'",
            timeout=240,
        )

    def wait_public_open(m):
        def ok(_):
            return not mcp(m, "public", "status")[0]
        retry(ok, timeout=dt.timedelta(seconds=240))

    def public_closed(m):
        """Every public tool fails with exactly the generic error."""
        for tool, args in [("search", {"query": "x"}), ("get", {"path": "/home/alice/x"}),
                           ("recent", {"days": 3650}), ("status", {})]:
            err, text = mcp(m, "public", tool, args)
            assert err and text == GENERIC, (tool, err, text)

    def set_config(m, text):
        """Replace the config atomically (new inode), as an editor would."""
        m.succeed(f"cat > /etc/llm-index/new.toml <<'EOF'\n{text}\nEOF")
        m.succeed("chmod 644 /etc/llm-index/new.toml && mv /etc/llm-index/new.toml /etc/llm-index/alice.toml")

    def toml(roots, exclude=()):
        out = 'state_dir = "/var/lib/llm-index/alice"\n'
        for name, path, tier in roots:
            out += f'[[root]]\nname = "{name}"\npath = "{path}"\ntier = "{tier}"\nkind = "local"\ninterval = "30s"\n'
        return out + "[exclude]\npaths = [" + ", ".join(f'"{e}"' for e in exclude) + "]\n"

    def write(m, path, text):
        b = base64.b64encode((text + "\n").encode()).decode()
        as_alice(m, f"mkdir -p $(dirname {path}) && echo {b} | base64 -d > {path}")

    def count_files(m, like):
        out = m.succeed(
            "sudo -u alice python3 -c \"import sqlite3; c=sqlite3.connect('file:/var/lib/llm-index/alice/private.db?mode=ro', uri=True); "
            f"print(c.execute(\\\"select count(*) from files where relpath like '{like}'\\\").fetchone()[0])\"")
        return int(out.strip())
  '';

  mk =
    {
      name,
      instance,
      extra ? { },
      script,
    }:
    pkgs.testers.runNixOSTest {
      inherit name;
      nodes.machine = node { inherit instance extra; };
      testScript = ''
        ${preamble}
        machine.wait_for_unit("multi-user.target")
        ${script}
      '';
    };

  fxRoot.roots.fx = localRoot "/home/alice/fx" "private";
in
{
  tier = mk {
    name = "llm-index-tier";
    instance.roots = {
      pub = localRoot "/home/alice/pub" "public";
      priv = localRoot "/home/alice/priv" "private";
    };
    script = ''
      write(machine, "/home/alice/pub/a.md", "# Apples\n\npubmarker apples are red")
      write(machine, "/home/alice/priv/b.md", "# Plans\n\nprivmarker the secretproject roadmap")
      machine.succeed("systemctl start llm-index-alice")
      wait_indexed(machine, "pub")
      wait_indexed(machine, "priv")
      wait_public_open(machine)
      assert "a.md" in mcp(machine, "public", "search", {"query": "pubmarker", "mode": "words"})[1]
      for q in ["privmarker", "secretproject", "roadmap"]:
          err, text = mcp(machine, "public", "search", {"query": q, "mode": "words"})
          assert not err and json.loads(text)["hits"] == [], (q, text)
      err, text = mcp(machine, "public", "get", {"path": "/home/alice/priv/b.md"})
      assert err and "priv" not in text, text
      for tool, args in [("search", {"query": "plans", "mode": "meaning"}), ("search", {"query": "privmarker"}), ("recent", {"days": 3650}), ("status", {})]:
          err, text = mcp(machine, "public", tool, args)
          assert not err, text
          for leak in ["priv", "b.md", "secretproject", "Plans"]:
              assert leak not in text, (tool, leak, text)
      st = json.loads(mcp(machine, "public", "status")[1])
      assert [r["name"] for r in st["roots"]] == ["pub"], st
      # the private server sees both
      assert "b.md" in mcp(machine, "private", "search", {"query": "privmarker"})[1]
      assert "a.md" in mcp(machine, "private", "search", {"query": "pubmarker"})[1]
      assert "priv" in status(machine)
    '';
  };

  freshness = mk {
    name = "llm-index-freshness";
    instance = fxRoot;
    script = ''
      write(machine, "/home/alice/fx/replaced.md", "oldcontentmarker lives here")
      machine.succeed("systemctl start llm-index-alice")
      wait_indexed(machine, "fx")
      out = as_alice(machine, f"python3 {TESTS}/freshness_session.py {EXE} {CFG} /home/alice/fx")
      assert "freshness ok" in out, out
    '';
  };

  reclassify = mk {
    name = "llm-index-reclassify";
    instance.roots = {
      pub = localRoot "/home/alice/pub" "public";
      pub2 = localRoot "/home/alice/pub2" "public";
      priv = localRoot "/home/alice/priv" "private";
    };
    script = ''
      BASE = [("pub", "/home/alice/pub", "public"), ("pub2", "/home/alice/pub2", "public"),
              ("priv", "/home/alice/priv", "private")]
      write(machine, "/home/alice/pub/a.md", "# Apples\n\npubmarker apples")
      write(machine, "/home/alice/pub/sub/s.md", "# Sub\n\nsubmarker nested")
      write(machine, "/home/alice/pub/skip/k.md", "# Skip\n\nskipmarker excluded")
      write(machine, "/home/alice/pub2/c.md", "# Pear\n\npub2marker pears")
      write(machine, "/home/alice/priv/b.md", "# Plans\n\nprivmarker plans")
      machine.succeed("systemctl start llm-index-alice")
      for r in ["pub", "pub2", "priv"]:
          wait_indexed(machine, r)
      wait_public_open(machine)

      def found(tier, q):
          err, text = mcp(machine, tier, "search", {"query": q, "mode": "words"})
          assert not err, text
          return json.loads(text)["hits"]

      assert found("public", "pubmarker") and found("public", "pub2marker") and found("public", "submarker")
      machine.succeed("systemctl stop llm-index-alice")

      # (a) public root -> private, indexer stopped: gone at once
      set_config(machine, toml([BASE[0], ("pub2", "/home/alice/pub2", "private"), BASE[2]]))
      assert not found("public", "pub2marker") and found("public", "pubmarker")
      assert mcp(machine, "public", "get", {"path": "/home/alice/pub2/c.md"})[0]
      assert found("private", "pub2marker")
      # (d) remove a root
      set_config(machine, toml([BASE[1], BASE[2]]))
      assert not found("public", "pubmarker") and found("public", "pub2marker")
      # (b) add a private child root under a public root: every public tool closed
      set_config(machine, toml(BASE + [("child", "/home/alice/pub/sub", "private")]))
      public_closed(machine)
      # (c) add an exclusion
      set_config(machine, toml(BASE, exclude=["/home/alice/pub/skip"]))
      public_closed(machine)
      # (e) corrupt config
      set_config(machine, "this is [[ not toml")
      public_closed(machine)
      assert mcp(machine, "private", "search", {"query": "x"})[0]

      # start the indexer: (b) and (c) validate, public reopens without the affected files
      set_config(machine, toml(BASE + [("child", "/home/alice/pub/sub", "private")], exclude=["/home/alice/pub/skip"]))
      public_closed(machine)
      machine.succeed("systemctl start llm-index-alice")
      wait_public_open(machine)
      assert found("public", "pubmarker")
      assert not found("public", "submarker"), "private child root leaked"
      assert not found("public", "skipmarker"), "excluded file leaked"

      # (f) an exclusion and (g) a private child root, each through a symlink
      machine.succeed("systemctl stop llm-index-alice")
      as_alice(machine, "mkdir -p /home/alice/pub/real/inner && ln -s real /home/alice/pub/link")
      write(machine, "/home/alice/pub/real/inner/r.md", "# Real\n\nrealmarker aliased")
      for cfgtext in [
          toml(BASE, exclude=["/home/alice/pub/link"]),
          toml(BASE + [("alias", "/home/alice/pub/link", "private")]),
      ]:
          set_config(machine, cfgtext)
          public_closed(machine)
          machine.succeed("systemctl start llm-index-alice")
          machine.wait_until_succeeds(
              f"sudo -u alice {EXE} status --config {CFG} | grep -F /home/alice/pub/link", timeout=240)
          machine.sleep(5)
          public_closed(machine)
          assert "/home/alice/pub/link" in mcp(machine, "private", "status")[1]
          machine.succeed("systemctl stop llm-index-alice")
    '';
  };

  sandbox =
    let
      instance = fxRoot;
    in
    pkgs.testers.runNixOSTest {
      name = "llm-index-sandbox";
      nodes.machine = node { inherit instance; };
      # the same module on a kernel booted without Landlock
      nodes.nolandlock = node {
        inherit instance;
        extra.security.lsm = lib.mkForce [ "yama" ];
      };
      testScript = ''
        ${preamble}
        start_all()
        machine.wait_for_unit("multi-user.target")
        write(machine, "/home/alice/fx/f.md", "# Fixture\n\nsandboxmarker indexed")
        machine.succeed("systemctl start llm-index-alice")
        machine.succeed("systemctl is-active llm-index-alice")
        # the self-test (ExecStartPre) passed
        show = machine.succeed("systemctl show llm-index-alice -p ExecStartPre")
        assert "code=exited ; status=0 }" in show, show
        assert "--self-test" in show, show
        wait_indexed(machine, "fx")
        assert "f.md" in mcp(machine, "private", "search", {"query": "sandboxmarker"})[1]

        # booted without Landlock: the unit does not start
        nolandlock.wait_for_unit("multi-user.target")
        assert "landlock" not in nolandlock.succeed("cat /sys/kernel/security/lsm")
        nolandlock.fail("systemctl start llm-index-alice")
        nolandlock.fail("systemctl is-active llm-index-alice")
        assert "Landlock" in nolandlock.succeed("journalctl -u llm-index-alice --no-pager")
      '';
    };

  access = mk {
    name = "llm-index-access";
    instance = fxRoot;
    script = ''
      write(machine, "/home/alice/fx/f.md", "# Fixture\n\naccessmarker text")
      machine.succeed("systemctl start llm-index-alice")
      wait_indexed(machine, "fx")
      # another user cannot read the state dir or the databases
      machine.fail("sudo -u bob ls /var/lib/llm-index/alice")
      machine.fail("sudo -u bob cat /var/lib/llm-index/alice/public.db")
      assert machine.succeed("stat -c %a /var/lib/llm-index/alice").strip() == "700"
      assert machine.succeed("stat -c %a /var/lib/llm-index/alice/private.db").strip() == "600"
      # no socket or listening port belongs to llm-index
      machine.succeed("! ss -H -lntup | grep -q llm-index")
      machine.succeed("! ss -H -lxp | grep -q llm-index")

      machine.succeed("systemctl stop llm-index-alice")
      # a state dir below .stfolder or on a network/FUSE-like filesystem is refused
      as_alice(machine, "mkdir -p /home/alice/synced && touch /home/alice/synced/.stfolder")
      machine.succeed("mkdir -p /tmp/shared && chmod 777 /tmp/shared")
      for state in ["/home/alice/synced/state", "/tmp/shared/llm-state"]:
          write(machine, "/home/alice/c.toml", f'state_dir = "{state}"\n[[root]]\nname = "fx"\npath = "/home/alice/fx"\n')
          status_, out = machine.execute(f"sudo -u alice {EXE} index --config /home/alice/c.toml 2>&1")
          assert status_ != 0, (state, out)
          assert "synced folder" in out or "network or FUSE" in out, (state, out)

      # with $HOME as a root, the state dir inside it is not indexed
      write(machine, "/home/alice/home.toml",
            'state_dir = "/home/alice/.local/state/llm-index"\n[[root]]\nname = "home"\npath = "/home/alice"\ninterval = "30s"\n')
      machine.succeed("sudo -u alice sh -c '(" + EXE + " index --config /home/alice/home.toml >/tmp/home.log 2>&1 &)'")
      machine.wait_until_succeeds(
          f"sudo -u alice {EXE} status --config /home/alice/home.toml | grep -E '^home .* ok .*queued/done: 0/[1-9]'",
          timeout=240)
      n = machine.succeed(
          "sudo -u alice python3 -c \"import sqlite3; c=sqlite3.connect('file:/home/alice/.local/state/llm-index/private.db?mode=ro', uri=True); "
          "print(c.execute(\\\"select count(*) from files where abspath like '%/.local/state/llm-index/%'\\\").fetchone()[0], "
          "c.execute('select count(*) from files').fetchone()[0])\"").split()
      assert n[0] == "0" and int(n[1]) > 0, n
    '';
  };

  watch = mk {
    name = "llm-index-watch";
    instance = fxRoot;
    script = ''
      def burst(prefix, n=100):
          as_alice(machine, f"mkdir -p /home/alice/fx/{prefix} && cd /home/alice/fx/{prefix} && "
                            f"for i in $(seq 1 {n}); do echo \"{prefix}marker $i\" > f$i.md; done")

      # directories beyond the watch limit
      as_alice(machine, "for i in $(seq 1 60); do mkdir -p /home/alice/fx/d$i; done; echo seed > /home/alice/fx/seed.md")
      machine.succeed("sysctl -w fs.inotify.max_user_watches=20")
      machine.succeed("systemctl start llm-index-alice")
      machine.wait_until_succeeds(f"sudo -u alice {EXE} status --config {CFG} | grep -E '^fx .* degraded '", timeout=240)
      assert "reason: " in status(machine)
      burst("watchlimit")
      retry(lambda _: count_files(machine, "watchlimit/%") == 100, timeout=dt.timedelta(seconds=300))

      # queue overflow
      machine.succeed("sysctl -w fs.inotify.max_user_watches=100000 fs.inotify.max_queued_events=16")
      machine.succeed("systemctl restart llm-index-alice")
      wait_indexed(machine, "fx")
      def overflowed(_):
          burst("overflow%d" % __import__("time").time_ns(), 300)
          return "degraded" in status(machine)
      retry(overflowed, timeout=dt.timedelta(seconds=240))
      burst("overflowfinal")
      retry(lambda _: count_files(machine, "overflowfinal/%") == 100, timeout=dt.timedelta(seconds=300))
    '';
  };

  # Two nodes: an NFS server and the client running the indexer. The client mounts NFS hard;
  # `server.block()` makes the share hang the way a dead NAS does.
  network =
    let
      share =
        path: tier:
        (localRoot path tier)
        // {
          kind = "network";
          share = "server:/";
        };
    in
    pkgs.testers.runNixOSTest {
      name = "llm-index-network";
      nodes.server = {
        networking.firewall.enable = false;
        services.nfs.server = {
          enable = true;
          exports = ''
            /export *(rw,no_root_squash,insecure,fsid=0)
            /export/alt *(rw,no_root_squash,insecure)
          '';
        };
      };
      nodes.client = node {
        instance = {
          roots = {
            nasa = share "/mnt/nas/a" "public";
            nasb = share "/mnt/nas/b" "private";
            loc = localRoot "/home/alice/loc" "private";
          };
          limits.backoffMax = "60s";
        };
        extra = {
          networking.firewall.enable = false;
          boot.supportedFilesystems = [ "nfs" ];
          environment.systemPackages = [ pkgs.nfs-utils ];
        };
      };
      testScript = ''
        ${preamble}
        import time
        start_all()
        server.wait_for_unit("nfs-server")
        client.wait_for_unit("multi-user.target")
        machine = client
        MOUNT = "mount -t nfs4 -o hard,timeo=30,retrans=2 server:/ /mnt/nas"
        START = "systemctl start llm-index-alice"
        STOP = "systemctl stop llm-index-alice"

        server.succeed("mkdir -p /export/a /export/b /export/alt/a /export/alt/b")
        server.succeed("echo 'nasamarker alpha' > /export/a/one.md; echo 'nasakeepmarker stays' > /export/a/keep.md; echo 'nasbmarker beta' > /export/b/two.md; echo 'otherexportmarker' > /export/alt/a/x.md")
        server.succeed("chmod -R a+rwX /export; exportfs -ra")
        client.succeed("mkdir -p /mnt/nas && " + MOUNT)
        write(client, "/home/alice/loc/l.md", "locmarker original")

        def fcount():
            out = client.succeed(
                "sudo -u alice python3 -c \"import sqlite3; "
                "print(sum(sqlite3.connect('file:/var/lib/llm-index/alice/%s.db?mode=ro' % d, uri=True)"
                ".execute('select count(*) from files').fetchone()[0] for d in ('public','private')))\"")
            return int(out.strip())

        def hits(tier, q):
            err, text = mcp(client, tier, "search", {"query": q, "mode": "words"})
            assert not err, text
            return json.loads(text)["hits"]

        def state(root):
            m = [l for l in status(client).splitlines() if l.startswith(root + " ")]
            return m[0] if m else ""

        def wait_state(root, word, timeout=300):
            retry(lambda _: word in state(root), timeout=dt.timedelta(seconds=timeout))

        client.succeed(START)
        for r in ["nasa", "nasb", "loc"]:
            wait_indexed(client, r)
        wait_public_open(client)
        assert hits("public", "nasakeepmarker") and hits("private", "nasbmarker")
        total = fcount()

        # --- the server hangs: three backoff cycles while the local root changes
        server.block()
        workers = []
        for i in range(36):
            if i % 12 == 0:
                write(client, "/home/alice/loc/l.md", f"locmarker edit{i}")
            n = client.succeed("ps -eo args | grep -c '[l]lm-index.*worker.*--root nas' || true").strip()
            workers.append(int(n))
            time.sleep(5)
        assert max(workers) <= 1, workers
        wait_state("nasa", "offline")
        assert hits("private", "nasamarker")
        assert all(h["stale"] for h in hits("private", "nasamarker")), "NFS hits must be stale"
        assert fcount() >= total, "nothing may be deleted while the share is away"
        retry(lambda _: bool(hits("private", "edit24")), timeout=dt.timedelta(seconds=120))

        # --- restore: a remote delete is seen only after a complete scan, a remote edit soon
        server.unblock()
        wait_state("nasa", " ok ")
        server.succeed("rm /export/a/one.md; echo 'remoteeditmarker' >> /export/b/two.md")
        retry(lambda _: bool(hits("private", "remoteeditmarker")), timeout=dt.timedelta(seconds=150))
        retry(lambda _: not hits("private", "nasamarker"), timeout=dt.timedelta(seconds=150))

        # --- a different export at the same path is an identity mismatch; nothing is deleted
        before = fcount()
        client.succeed("umount -l /mnt/nas && mount -t nfs4 -o hard,timeo=30,retrans=2 server:/alt /mnt/nas")
        wait_state("nasa", "identity mismatch", timeout=240)
        assert fcount() >= before
        client.succeed("umount -l /mnt/nas && " + MOUNT)
        wait_state("nasa", " ok ", timeout=300)

        # --- reclassify with the share hung: (a) and (d) apply at once, (b) keeps public closed
        client.succeed(STOP)
        server.block()
        def nconf(roots):
            out = 'state_dir = "/var/lib/llm-index/alice"\n'
            for name, path, tier, kind in roots:
                out += f'[[root]]\nname = "{name}"\npath = "{path}"\ntier = "{tier}"\nkind = "{kind}"\ninterval = "30s"\n'
                if kind == "network":
                    out += 'share = "server:/"\n'
            return out
        A = ("nasa", "/mnt/nas/a", "public", "network")
        B = ("nasb", "/mnt/nas/b", "private", "network")
        L = ("loc", "/home/alice/loc", "private", "local")
        set_config(client, nconf([("nasa", "/mnt/nas/a", "private", "network"), B, L]))   # (a)
        assert not hits("public", "nasakeepmarker")
        set_config(client, nconf([B, L]))                                                  # (d)
        assert not hits("public", "nasbmarker")
        set_config(client, nconf([A, B, L, ("child", "/mnt/nas/a/sub", "private", "network")]))   # (b)
        public_closed(client)
        client.succeed(START)
        client.sleep(90)
        public_closed(client)   # still closed: the hung share cannot be validated
        server.unblock()
        wait_public_open(client)
      '';
    };

  # Mount cases: automount, a tmpfs over a subdirectory, dead shares next to a healthy one,
  # a share mounted after `index` started, and reclassify with the share unmounted.
  # `server` hosts the shares that die, `server2` the healthy one.
  network-mounts =
    let
      share =
        path: src: interval:
        (localRoot path "private")
        // {
          kind = "network";
          share = src;
          inherit interval;
        };
      nfsServer = {
        networking.firewall.enable = false;
        services.nfs.server = {
          enable = true;
          exports = ''
            /export *(rw,no_root_squash,insecure,fsid=0)
            /export/alt *(rw,no_root_squash,insecure)
          '';
        };
      };
    in
    pkgs.testers.runNixOSTest {
      name = "llm-index-network-mounts";
      nodes.server = nfsServer;
      nodes.server2 = nfsServer;
      nodes.client = node {
        instance = {
          roots = {
            auto = (share "/mnt/auto/autodir" "server2:/" "30s") // {
              kind = "auto";
            };
            ok = share "/mnt/ok/okdir" "server2:/" "30s";
            d1 = share "/mnt/d1/d1dir" "server:/" "30s";
            d2 = share "/mnt/d2" "server:/alt" "30s";
            late = share "/mnt/late" "server2:/late" "1d";
          };
          limits.backoffMax = "60s";
        };
        extra = {
          networking.firewall.enable = false;
          boot.supportedFilesystems = [ "nfs" ];
          environment.systemPackages = [ pkgs.nfs-utils ];
          systemd.mounts = [
            {
              what = "server2:/";
              where = "/mnt/auto";
              type = "nfs4";
              options = "hard";
            }
          ];
          systemd.automounts = [
            {
              where = "/mnt/auto";
              wantedBy = [ "multi-user.target" ];
            }
          ];
        };
      };
      testScript = ''
        ${preamble}
        start_all()
        server.wait_for_unit("nfs-server")
        server2.wait_for_unit("nfs-server")
        client.wait_for_unit("multi-user.target")
        machine = client
        START = "systemctl start llm-index-alice"
        STOP = "systemctl stop llm-index-alice"
        NFS = "mount -t nfs4 -o hard,timeo=30,retrans=2"

        for s in (server, server2):
            s.succeed("mkdir -p /export/alt /export/late /export/pub/sub /export/pub/skip /export/autodir /export/okdir/sub /export/d1dir")
        server2.succeed("echo automarker > /export/autodir/x.md; echo okonemarker > /export/okdir/ok1.md; echo insubmarker > /export/okdir/sub/in.md; echo latemarker > /export/late/l.md")
        server2.succeed("echo pubsharemarker > /export/pub/a.md; echo submarker > /export/pub/sub/s.md; echo skipmarker > /export/pub/skip/k.md")
        server.succeed("echo d1marker > /export/d1dir/y.md; echo d2marker > /export/alt/z.md")
        for s in (server, server2):
            s.succeed("chmod -R a+rwX /export; exportfs -ra")
        client.succeed("mkdir -p /mnt/ok /mnt/d1 /mnt/d2 /mnt/late /mnt/pubshare")
        client.succeed(f"{NFS} server2:/ /mnt/ok && {NFS} server:/ /mnt/d1 && {NFS} server:/alt /mnt/d2")

        def fcount():
            out = client.succeed(
                "sudo -u alice python3 -c \"import sqlite3; "
                "print(sum(sqlite3.connect('file:/var/lib/llm-index/alice/%s.db?mode=ro' % d, uri=True)"
                ".execute('select count(*) from files').fetchone()[0] for d in ('public','private')))\"")
            return int(out.strip())

        def hits(tier, q):
            err, text = mcp(client, tier, "search", {"query": q, "mode": "words"})
            assert not err, text
            return json.loads(text)["hits"]

        def state(root):
            m = [l for l in status(client).splitlines() if l.startswith(root + " ")]
            return m[0] if m else ""

        def seconds(n):
            return dt.timedelta(seconds=n)

        # the automount point itself; the NFS mount appears on first access
        client.succeed("systemctl start mnt-auto.automount")
        assert "autofs" in client.succeed("findmnt -n -o FSTYPE /mnt/auto")
        client.succeed(START)

        # (1) an automount share indexes like a plain mount
        for r in ["auto", "ok", "d1", "d2"]:
            try:
                client.wait_until_succeeds(
                    f"sudo -u alice {EXE} status --config {CFG} | grep -E '^{r} .* ok .*queued/done: 0/[1-9]'",
                    timeout=90)
            except Exception:
                print("DIAG status:\n" + status(client))
                print("DIAG journal:\n" + client.succeed("journalctl -u llm-index-alice --no-pager | tail -30"))
                print("DIAG mounts:\n" + client.succeed("grep -E 'mnt' /proc/self/mountinfo"))
                raise
        assert hits("private", "automarker"), state("auto")
        assert "nfs4" in client.succeed("findmnt -n -o FSTYPE /mnt/auto")
        assert hits("private", "okonemarker") and hits("private", "d1marker") and hits("private", "d2marker")

        # (2) an empty tmpfs over a subdirectory of the indexed share deletes nothing
        before = fcount()
        client.succeed("mount -t tmpfs tmpfs /mnt/ok/okdir/sub")
        client.sleep(100)   # at least two scans
        print("state with tmpfs over a subdirectory:", state("ok"))
        assert fcount() >= before
        assert hits("private", "insubmarker"), "files below the covered subdirectory were deleted"
        client.succeed("umount /mnt/ok/okdir/sub")

        # (3) two dead shares: the healthy one keeps being scanned
        server.block()
        retry(lambda _: "offline" in state("d1") and "offline" in state("d2"), timeout=seconds(300))
        server2.succeed("echo deadsharemarker > /export/okdir/new.md")
        retry(lambda _: bool(hits("private", "deadsharemarker")), timeout=seconds(150))
        assert " ok " in state("ok"), state("ok")
        server.unblock()

        # (4) a share mounted after `index` started is scanned at once (interval is 1 day)
        assert not hits("private", "latemarker")
        client.succeed(f"{NFS} server2:/late /mnt/late")
        try:
            retry(lambda _: bool(hits("private", "latemarker")), timeout=seconds(60))
        except Exception:
            print("DIAG status:\n" + status(client))
            print("DIAG journal:\n" + client.succeed("journalctl -u llm-index-alice --no-pager | tail -60"))
            raise

        # (5) reclassify (a)-(c) with the share unmounted
        client.succeed(f"{NFS} server2:/pub /mnt/pubshare")
        def nconf(roots, exclude=()):
            out = 'state_dir = "/var/lib/llm-index/alice"\n'
            for name, path, tier in roots:
                out += f'[[root]]\nname = "{name}"\npath = "{path}"\ntier = "{tier}"\nkind = "network"\ninterval = "30s"\nshare = "server2:/pub"\n'
            return out + "[exclude]\npaths = [" + ", ".join(f'"{e}"' for e in exclude) + "]\n"
        P = ("ps", "/mnt/pubshare", "public")
        client.succeed(STOP)
        set_config(client, nconf([P]))
        client.succeed(START)
        wait_indexed(client, "ps")
        wait_public_open(client)
        assert hits("public", "pubsharemarker") and hits("public", "submarker") and hits("public", "skipmarker")
        client.succeed(STOP)
        client.succeed("umount -l /mnt/pubshare")
        set_config(client, nconf([("ps", "/mnt/pubshare", "private")]))         # (a)
        assert not hits("public", "pubsharemarker")
        set_config(client, nconf([]))                                            # (d)
        assert not hits("public", "pubsharemarker")
        set_config(client, nconf([P, ("child", "/mnt/pubshare/sub", "private")]))   # (b)
        public_closed(client)
        set_config(client, nconf([P], exclude=["/mnt/pubshare/skip"]))           # (c)
        public_closed(client)
        set_config(client, nconf([P, ("child", "/mnt/pubshare/sub", "private")], exclude=["/mnt/pubshare/skip"]))
        public_closed(client)
        client.succeed(START)   # validation succeeds on the mount point of the unmounted share
        wait_public_open(client)
        assert hits("public", "pubsharemarker")
        assert not hits("public", "submarker"), "private child root leaked"
        assert not hits("public", "skipmarker"), "excluded file leaked"
      '';
    };
}
