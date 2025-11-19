let
  # CDK Admin SSH public keys
  admin1 = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJiA6Oq79afOa48iyOVfs7iVbs3Ug9Elj8GdtWLs2UcD tsk@thesimplekid.com";

  users = [
    admin1
  ];

  # Server SSH host keys (add after bootstrap)
  # After bootstrapping, run: ssh root@SERVER cat /etc/ssh/ssh_host_ed25519_key.pub
  cdk-runner-01 = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFVbjF/9l23hl6knw0aAHb9/mZz1dcoTEVTJEHRrcxXe root@cdk-runner-01";

  runners = [
    cdk-runner-01  # TODO: Uncomment after adding host key above
  ];
in
{
  "secrets/github-runner.age".publicKeys = runners ++ users;
}
