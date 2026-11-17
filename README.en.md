# Virtum

[Français](./README.md) | **English**

**VMs, made simple.**

Virtum runs local virtual machines and multi-VM labs with a Docker-like experience: `virtum run`, `virtum ps`, `virtum shell`, and a `Virtumfile` to describe a whole lab. VMs run on KVM and libvirt, on your own Linux machine.

```bash
virtum run --os debian-12 --name web   # a Debian VM, ready in about a minute
virtum shell web                       # SSH session
virtum up                              # the lab described in ./Virtumfile
```

## Why Virtum

- **Simple**: one command for a VM, one YAML file for a lab. No Ruby Vagrantfile, no web console.
- **Fast**: official cloud images, copy-on-write disks, machines created in parallel.
- **Reproducible**: `plan` shows what will change, `apply` applies it, snapshots bring a lab back to a known state.
- **Secure by default**: read-only shares, SSH host key pinned per VM, local-only ports unless exposure is confirmed. See [SECURITY.md](./SECURITY.md).
- **Open source**: Apache-2.0 license.

## Installation

Requirements: Linux x86_64 with KVM (virtualization enabled in the BIOS/UEFI). Tested on Ubuntu 24.04; the binary needs glibc 2.34 or newer (Debian 12, Ubuntu 22.04 and later), but these systems have not been validated end to end yet.

### From a release (recommended)

Download `virtum-<version>-x86_64-linux.tar.gz` and `SHA256SUMS` from the [releases page](https://github.com/Marvinviverge/virtum/releases), then:

```bash
sha256sum -c SHA256SUMS                     # the archive has not been altered
gh attestation verify virtum-*-x86_64-linux.tar.gz -R Marvinviverge/virtum   # optional: built by the repository's CI
tar xzf virtum-*-x86_64-linux.tar.gz && cd virtum-*-x86_64-linux
sudo ./scripts/setup-host.sh                # once per machine: packages, libvirt, groups
install -D -m 755 virtum ~/.local/bin/virtum
sudo ./scripts/setup-var-lib.sh             # prepares /var/lib/virtum
virtum doctor
```

`~/.local/bin` must be in your `PATH`. After `setup-host.sh`, log in again to join the `libvirt` and `kvm` groups. Every `virtum doctor` check must be `[ok]`. `gh attestation verify` needs GitHub CLI 2.49 or newer.

### From source

With [Rust](https://rustup.rs) installed:

```bash
sudo ./scripts/setup-host.sh
make install                  # builds and installs into ~/.cargo/bin
sudo ./scripts/setup-var-lib.sh
virtum doctor
```

Packages installed by `setup-host.sh` (Debian/Ubuntu): `qemu-kvm`, `libvirt-daemon-system`, `libvirt-clients`, `qemu-utils`, `cloud-image-utils`, `openssh-client`, `socat`, `virtiofsd`, `bubblewrap`. On another distribution, install their equivalents and add your account to the `libvirt` and `kvm` groups.

## Quick start

### A single VM

```bash
virtum run --os debian-12 --name web              # Debian 12 (also: ubuntu-24.04, ubuntu-26.04)
virtum run --os ubuntu-24.04 --ram 4096 --cpu 2 --disk-size 20G
virtum ps                                         # running VMs and their IP; -a for all
virtum shell web
virtum stop web && virtum start web
virtum destroy web
```

The first use of an OS downloads its cloud image (`virtum pull <os>` does it ahead of time).

### A lab

A `Virtumfile` describes several machines on an isolated network:

```yaml
name: demo-lab

machines:
  web:
    os: debian-12
    ram: 2048
    cpu: 2
    ports:
      - "18080:80"          # http://127.0.0.1:18080 → port 80 of the VM
    provision:
      - apt-get update
      - apt-get install -y nginx

  db:
    os: ubuntu-24.04
    ram: 1024
```

```bash
virtum plan                    # what will be created or changed, without touching anything
virtum up                      # creates or restarts the lab
virtum shell web
virtum down                    # stops the lab; --destroy to delete it
```

Full example: [`examples/Virtumfile`](./examples/Virtumfile).

## Features

### Commands

| Command | Purpose |
| :--- | :--- |
| `doctor` | Checks that the host is ready (KVM, libvirt, groups, storage, tools) |
| `run`, `ps`, `shell`, `stop`, `start`, `destroy` | VM lifecycle |
| `pull` | Downloads an OS image into the cache |
| `plan`, `up`, `apply`, `down` | Lifecycle of a lab described by a Virtumfile |
| `snapshot create\|restore\|ls\|rm` | Snapshots of a VM or a whole lab |
| `inventory`, `provision` | The lab's Ansible inventory and playbook |
| `open`, `close` | Host firewall (ufw) for ports exposed to the local network |

`virtum <command> --help` details every option.

### Networking

- Each lab has its own **isolated libvirt network** (`virtum-lab-<name>`); machines in a lab can reach each other.
- `subnet: 10.42.0.0/24` sets the lab subnet; otherwise it is derived from the lab name. `plan` and `up` refuse a subnet that overlaps an existing network.
- `ip: 10.42.0.20` gives a machine a fixed address.
- `ports:` forwards a host port to the VM, on `127.0.0.1`. With `bind: lan`, it listens on the local network after confirmation; `virtum open` then opens the firewall.
- **Bridge**: `virtum run --network bridge` or `network: bridge` in a lab puts the VM directly on the local network, with its own address. Ethernet is recommended, Wi-Fi is often incompatible. A lab can mix bridged and private machines.

### Shared folders

```yaml
machines:
  web:
    os: debian-12
    mounts:
      - ./src:~/src             # read-only (default)
      - ./out:~/out:rw          # the VM can write
```

Shares use virtiofs. Read-only access is enforced on the host. A `rw` share cannot contain the Virtumfile or the Ansible files, and shares cannot be nested. `down --destroy --purge` also deletes the folders created for shares.

### Ansible

```yaml
ansible:
  playbook: ansible/site.yml
  requirements: ansible/requirements.yml   # optional

machines:
  web:
    os: debian-12
    groups: [front]
```

`virtum up` runs the playbook on the first deployment, then takes the `provisioned` lab snapshot (`virtum snapshot restore provisioned` brings you back to it). `virtum provision` runs it again, and `virtum inventory` prints the inventory (INI or YAML) with each VM's SSH keys. The playbook runs from a private copy of its folder, without the writable shares.

### Snapshots

```bash
virtum snapshot create before-test     # the whole lab (or: before-test web db)
virtum snapshot restore before-test
virtum snapshot ls
virtum snapshot rm before-test
```

Cold disk snapshots: VMs are stopped during the operation, then restarted.

### Other Virtumfile options

| Field | Purpose |
| :--- | :--- |
| `ram`, `cpu`, `disk_size` | Resources; `apply` updates them on existing VMs |
| `provision:` | Shell commands run at first boot |
| `ssh_port:` | SSH port if the VM moves it (falls back to 22) |
| `restart: no \| always \| unless-stopped` | Start at host boot, as in Docker Compose |

Removing a machine from the Virtumfile does not delete it: `plan` reports it, `apply --prune` deletes it.

## Storage

Virtum uses the system libvirt (`qemu:///system`) and keeps images, instances, keys and state in `/var/lib/virtum` (prepared by `setup-var-lib.sh`), otherwise in `~/.virtum`.

## Security

Threat model, guarantees and limits: [SECURITY.md](./SECURITY.md) (in French). To report a vulnerability: **security@virtum.dev**.

## Contributing

Contributions are welcome: see [CONTRIBUTING.en.md](./CONTRIBUTING.en.md). `make check` runs formatting, clippy, the tests and the dependency audit; `make smoke-lab` checks a real lab end to end.

## License

Virtum is distributed under the [Apache 2.0](./LICENSE) license: free to use, modify and redistribute, including commercially, as long as the license and copyright notices are kept. Release history: [CHANGELOG.md](./CHANGELOG.md) (in French).
