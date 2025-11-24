{ lib, pkgs, hostName, runnerGroup, runners, ... }:

let
  runnersNames = map (name: "${hostName}-${name}") runners;
in
{

  # Use disk-based /tmp instead of tmpfs for GitHub Actions builds
  boot.tmp.useTmpfs = false;  # Use disk instead of RAM
  boot.tmp.cleanOnBoot = true;


  environment.systemPackages = map lib.lowPrio [
  ];

  age.secrets = {
    github-runner-token = {
      file = ../../secrets/github-runner.age;
      path = "/run/secrets/github-runner/token";
      # Note: this doesn't need to be readable to the runner user(s)
      # Seems like NixOS script will register with GH first, then
      # allow runner user to see only the post-registration creds,
      # which is much safer.
      owner = "root";
      group = "root";
      mode = "600";
    };
  };

  users.groups.github-runner = { };
  users.users.github-runner = {
    # behaves as normal user, needs a shell and home
    isNormalUser = true;
    group = "github-runner";
    home = "/home/github-runner";
    extraGroups = [ "docker" ];
  };

  virtualisation.docker.enable = true;

  # Create directories on disk for GitHub Actions builds
  systemd.tmpfiles.rules =
    let
      runnerWorkDirs = map (name: "d /var/lib/github-runner-work/${name} 0755 github-runner github-runner -") runnersNames;
    in
    [
      "d /home/github-runner/tmp 0755 github-runner github-runner -"
      "d /home/github-runner/.cache 0755 github-runner github-runner -"
      "d /var/lib/github-runner-work 0755 github-runner github-runner -"
    ] ++ runnerWorkDirs;

  services.github-runners = lib.listToAttrs (map
    (name: {
      inherit name;
      value = {
        enable = true;
        # this will shut down the whole service after every run,
        # notably making sure there is no ghosts processes
        ephemeral = true;
        replace = true;
        inherit name;
        url = "https://github.com/thesimplekid/cdk";
        tokenFile = "/run/secrets/github-runner/token";
        user = "github-runner";
        extraLabels = [ "self-hosted" "ci" "nix" "x64" "Linux" ];
        # Use disk-based working directory instead of /run tmpfs
        workDir = "/var/lib/github-runner-work/${name}";
        serviceOverrides = {
          # To access /var/run/docker.sock we need to be part of docker group,
          # but it doesn't seem to work when it's mapped as `nobody` due to `PrivateUsers=true`
          PrivateUsers = false;

          # Some tools need access to real home directory
          ProtectHome = false;

          # Disable mount namespace to avoid /run restrictions
          # This allows the runner to use more space for builds
          PrivateMounts = false;

          # These are hard to wipe, break cachix, break `--keep-failed-build`, etc.
          PrivateTmp = false;
          ProtectSystem = "full"; # instead of "strict", to make /tmp actually usable

          # Shared cache directory for consistent builds
          # Redirect temp directories to disk instead of /run tmpfs
          Environment = [
            "RUNNER_CACHE_DIR=/home/github-runner/.cache"
            "TMPDIR=/home/github-runner/tmp"
            "RUNNER_TEMP=/home/github-runner/tmp"
            "TEMP=/home/github-runner/tmp"
            "TMP=/home/github-runner/tmp"
          ];

          # Browser support for integration tests
          # removed "~capset"
          SystemCallFilter = lib.mkForce [
            "~@clock"
            "~@cpu-emulation"
            "~@module"
            "~@mount"
            "~@obsolete"
            "~@raw-io"
            "~@reboot"
            "~setdomainname"
            "~sethostname"
          ];
          CapabilityBoundingSet = [ "CAP_SETUID" "CAP_SETGID" "CAP_SYS_ADMIN" ];
          NoNewPrivileges = false;

          # Apparently it wasn't restarting on failure, so let's make sure it does
          Restart = lib.mkForce "always";
          RestartSec = "5s";  # Reduced from 30s for faster job pickup after ephemeral runner completes
        };
        extraPackages = with pkgs; [
          gawk
          docker
          cachix
          gnupg
          curl
          jq
          xz
        ];
      };
    })
    runnersNames);

}
